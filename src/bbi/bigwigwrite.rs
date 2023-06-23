use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};

use futures::executor::{block_on, ThreadPool};
use futures::future::FutureExt;
use futures::sink::SinkExt;
use futures::task::SpawnExt;

use byteorder::{NativeEndian, WriteBytesExt};

use crate::utils::chromvalues::ChromValues;
use crate::utils::tell::Tell;
use crate::ChromData;

use crate::bbi::{Summary, Value, ZoomRecord, BIGWIG_MAGIC};
use crate::bbiwrite::{
    self, encode_zoom_section, get_rtreeindex, write_blank_headers, write_chrom_tree,
    write_rtreeindex, write_zooms, BBIWriteOptions, ChromProcessingInput, ProcessChromError,
    SectionData,
};

pub struct BigWigWrite {
    pub path: String,
    pub options: BBIWriteOptions,
}

impl BigWigWrite {
    pub fn create_file(path: String) -> Self {
        BigWigWrite {
            path,
            options: BBIWriteOptions::default(),
        }
    }

    pub fn write<
        Values: ChromValues<Value = Value> + Send + 'static,
        V: ChromData<ProcessChromError<Values::Error>, Output = Values>,
    >(
        self,
        chrom_sizes: HashMap<String, u32>,
        vals: V,
        pool: ThreadPool,
    ) -> Result<(), ProcessChromError<Values::Error>> {
        let fp = File::create(self.path.clone())?;
        let mut file = BufWriter::new(fp);

        write_blank_headers(&mut file)?;

        let total_summary_offset = file.tell()?;
        file.write_all(&[0; 40])?;

        let full_data_offset = file.tell()?;

        // Total items
        // Unless we know the vals ahead of time, we can't estimate total sections ahead of time.
        // Even then simply doing "(vals.len() as u32 + ITEMS_PER_SLOT - 1) / ITEMS_PER_SLOT"
        // underestimates because sections are split by chrom too, not just size.
        // Skip for now, and come back when we write real header + summary.
        file.write_u64::<NativeEndian>(0)?;

        let pre_data = file.tell()?;
        // Write data to file and return
        let (chrom_ids, summary, mut file, raw_sections_iter, zoom_infos, uncompress_buf_size) =
            block_on(bbiwrite::write_vals(
                vals,
                file,
                self.options,
                BigWigWrite::process_chrom,
                pool,
                chrom_sizes.clone(),
            ))?;
        let data_size = file.tell()? - pre_data;
        let mut current_offset = pre_data;
        let sections_iter = raw_sections_iter.map(|mut section| {
            // TODO: this assumes that all the data is contiguous
            // This will fail if we ever space the sections in any way
            section.offset = current_offset;
            current_offset += section.size;
            section
        });

        // Since the chrom tree is read before the index, we put this before the full data index
        // Therefore, there is a higher likelihood that the udc file will only need one read for chrom tree + full data index
        // Putting the chrom tree before the data also has a higher likelihood of being included with the beginning headers,
        // but requires us to know all the data ahead of time (when writing)
        let chrom_index_start = file.tell()?;
        write_chrom_tree(&mut file, chrom_sizes, &chrom_ids.get_map())?;

        let index_start = file.tell()?;
        let (nodes, levels, total_sections) = get_rtreeindex(sections_iter, self.options);
        write_rtreeindex(&mut file, nodes, levels, total_sections, self.options)?;

        let zoom_entries = write_zooms(&mut file, zoom_infos, data_size, self.options)?;
        let num_zooms = zoom_entries.len() as u16;

        file.seek(SeekFrom::Start(0))?;
        file.write_u32::<NativeEndian>(BIGWIG_MAGIC)?;
        file.write_u16::<NativeEndian>(4)?; // Actually 3, unsure what version 4 actually adds
        file.write_u16::<NativeEndian>(num_zooms)?;
        file.write_u64::<NativeEndian>(chrom_index_start)?;
        file.write_u64::<NativeEndian>(full_data_offset)?;
        file.write_u64::<NativeEndian>(index_start)?;
        file.write_u16::<NativeEndian>(0)?; // fieldCount
        file.write_u16::<NativeEndian>(0)?; // definedFieldCount
        file.write_u64::<NativeEndian>(0)?; // autoSQLOffset
        file.write_u64::<NativeEndian>(total_summary_offset)?;
        file.write_u32::<NativeEndian>(uncompress_buf_size as u32)?;
        file.write_u64::<NativeEndian>(0)?; // reserved

        debug_assert!(file.seek(SeekFrom::Current(0))? == 64);

        for zoom_entry in zoom_entries {
            file.write_u32::<NativeEndian>(zoom_entry.reduction_level)?;
            file.write_u32::<NativeEndian>(0)?;
            file.write_u64::<NativeEndian>(zoom_entry.data_offset)?;
            file.write_u64::<NativeEndian>(zoom_entry.index_offset)?;
        }

        file.seek(SeekFrom::Start(total_summary_offset))?;
        file.write_u64::<NativeEndian>(summary.bases_covered)?;
        file.write_f64::<NativeEndian>(summary.min_val)?;
        file.write_f64::<NativeEndian>(summary.max_val)?;
        file.write_f64::<NativeEndian>(summary.sum)?;
        file.write_f64::<NativeEndian>(summary.sum_squares)?;

        file.seek(SeekFrom::Start(full_data_offset))?;
        file.write_u64::<NativeEndian>(total_sections)?;
        file.seek(SeekFrom::End(0))?;
        file.write_u32::<NativeEndian>(BIGWIG_MAGIC)?; // TODO: see above, should encode with NativeEndian

        Ok(())
    }

    pub(crate) async fn process_chrom<I: ChromValues<Value = Value>>(
        processing_input: ChromProcessingInput,
        chrom_id: u32,
        options: BBIWriteOptions,
        pool: ThreadPool,
        mut chrom_values: I,
        chrom: String,
        chrom_length: u32,
    ) -> Result<Summary, ProcessChromError<I::Error>> {
        let ChromProcessingInput {
            mut zooms_channels,
            mut ftx,
        } = processing_input;

        struct ZoomItem {
            // How many bases this zoom item covers
            size: u32,
            // The current zoom entry
            live_info: Option<ZoomRecord>,
            // All zoom entries in the current section
            records: Vec<ZoomRecord>,
        }
        struct BedGraphSection {
            items: Vec<Value>,
            zoom_items: Vec<ZoomItem>,
        }

        let mut summary = Summary {
            total_items: 0,
            bases_covered: 0,
            min_val: f64::MAX,
            max_val: f64::MIN,
            sum: 0.0,
            sum_squares: 0.0,
        };

        let mut state_val = BedGraphSection {
            items: Vec::with_capacity(options.items_per_slot as usize),
            zoom_items: std::iter::successors(Some(options.initial_zoom_size), |z| Some(z * 4))
                .take(options.max_zooms as usize)
                .map(|size| ZoomItem {
                    size,
                    live_info: None,
                    records: Vec::with_capacity(options.items_per_slot as usize),
                })
                .collect(),
        };
        while let Some(current_val) = chrom_values.next() {
            // If there is a source error, propogate that up
            let current_val = current_val.map_err(ProcessChromError::SourceError)?;

            // Check a few preconditions:
            // - The current end is greater than or equal to the start
            // - The current end is at most the chromosome length
            // - If there is a next value, then it does not overlap value
            // TODO: test these correctly fails
            if current_val.start > current_val.end {
                return Err(ProcessChromError::InvalidInput(format!(
                    "Invalid bed graph: {} > {}",
                    current_val.start, current_val.end
                )));
            }
            if current_val.end > chrom_length {
                return Err(ProcessChromError::InvalidInput(format!(
                    "Invalid bed graph: `{}` is greater than the chromosome ({}) length ({})",
                    current_val.end, chrom, chrom_length
                )));
            }
            match chrom_values.peek() {
                None | Some(Err(_)) => (),
                Some(Ok(next_val)) => {
                    if current_val.end > next_val.start {
                        return Err(ProcessChromError::InvalidInput(format!(
                            "Invalid bed graph: overlapping values on chromosome {} at {}-{} and {}-{}",
                            chrom,
                            current_val.start,
                            current_val.end,
                            next_val.start,
                            next_val.end,
                        )));
                    }
                }
            }

            // Now, actually process the value.

            // First, update the summary.
            let len = current_val.end - current_val.start;
            let val = f64::from(current_val.value);
            summary.total_items += 1;
            summary.bases_covered += u64::from(len);
            summary.min_val = summary.min_val.min(val);
            summary.max_val = summary.max_val.max(val);
            summary.sum += f64::from(len) * val;
            summary.sum_squares += f64::from(len) * val * val;

            // Then, add the item to the zoom item queues. This is a bit complicated.
            for (zoom_item, zoom_channel) in std::iter::zip(state_val.zoom_items.iter_mut(), zooms_channels.iter_mut()) {
                debug_assert_ne!(zoom_item.records.len(), options.items_per_slot as usize);

                // Zooms are comprised of a tiled set of summaries. Each summary spans a fixed length.
                // Zoom summaries are compressed similarly to main data, with a given items per slot.
                // It may be the case that our value spans across multiple zoom summaries, so this inner loop handles that.

                // `add_start` indicates where we are *currently* adding bases from (either the start of this item or in the middle, but beginning of another zoom section)
                let mut add_start = current_val.start;
                loop {
                    // Write section if full; or if no next section, some items, and no current zoom record
                    if (add_start >= current_val.end
                        && zoom_item.live_info.is_none()
                        && chrom_values.peek().is_none()
                        && !zoom_item.records.is_empty())
                        || zoom_item.records.len() == options.items_per_slot as usize
                    {
                        let items = std::mem::take(&mut zoom_item.records);
                        let handle = pool
                            .spawn_with_handle(encode_zoom_section(options.compress, items))
                            .expect("Couldn't spawn.");
                        zoom_channel
                            .send(handle.boxed())
                            .await
                            .expect("Couln't send");
                    }
                    if add_start >= current_val.end {
                        if chrom_values.peek().is_none() {
                            if let Some(zoom2) = zoom_item.live_info.take() {
                                zoom_item.records.push(zoom2);
                                continue;
                            }
                        }
                        break;
                    }
                    let zoom2 = zoom_item.live_info.get_or_insert(ZoomRecord {
                        chrom: chrom_id,
                        start: add_start,
                        end: add_start,
                        summary: Summary {
                            total_items: 0,
                            bases_covered: 0,
                            min_val: val,
                            max_val: val,
                            sum: 0.0,
                            sum_squares: 0.0,
                        },
                    });
                    // The end of zoom record
                    let next_end = zoom2.start + zoom_item.size;
                    // End of bases that we could add
                    let add_end = std::cmp::min(next_end, current_val.end);
                    // If the last zoom ends before this value starts, we don't add anything
                    if add_end >= add_start {
                        let added_bases = add_end - add_start;
                        zoom2.end = add_end;
                        zoom2.summary.total_items += 1;
                        zoom2.summary.bases_covered += u64::from(added_bases);
                        zoom2.summary.min_val = zoom2.summary.min_val.min(val);
                        zoom2.summary.max_val = zoom2.summary.max_val.max(val);
                        zoom2.summary.sum += f64::from(added_bases) * val;
                        zoom2.summary.sum_squares += f64::from(added_bases) * val * val;
                    }
                    // If we made it to the end of the zoom (whether it was because the zoom ended before this value started,
                    // or we added to the end of the zoom), then write this zooms to the current section
                    if add_end == next_end {
                        zoom_item.records.push(zoom_item.live_info.take().unwrap());
                    }
                    // Set where we would start for next time
                    add_start = add_end;
                }
                debug_assert_ne!(zoom_item.records.len(), options.items_per_slot as usize);
            }
            // Then, add the current item to the actual values, and encode if full, or last item
            state_val.items.push(current_val);
            if chrom_values.peek().is_none()
                || state_val.items.len() >= options.items_per_slot as usize
            {
                let items = std::mem::take(&mut state_val.items);
                let handle = pool
                    .spawn_with_handle(encode_section(options.compress, items, chrom_id))
                    .expect("Couldn't spawn.");
                ftx.send(handle.boxed()).await.expect("Couldn't send");
            }
        }

        debug_assert!(state_val.items.is_empty());
        for zoom_item in state_val.zoom_items.iter_mut() {
            debug_assert!(zoom_item.live_info.is_none());
            debug_assert!(zoom_item.records.is_empty());
        }

        if summary.total_items == 0 {
            summary.min_val = 0.0;
            summary.max_val = 0.0;
        }
        Ok(summary)
    }
}

async fn encode_section(
    compress: bool,
    items_in_section: Vec<Value>,
    chrom_id: u32,
) -> io::Result<(SectionData, usize)> {
    use libdeflater::{CompressionLvl, Compressor};

    let mut bytes = Vec::with_capacity(24 + (items_in_section.len() * 24));

    let start = items_in_section[0].start;
    let end = items_in_section[items_in_section.len() - 1].end;
    bytes.write_u32::<NativeEndian>(chrom_id)?;
    bytes.write_u32::<NativeEndian>(start)?;
    bytes.write_u32::<NativeEndian>(end)?;
    bytes.write_u32::<NativeEndian>(0)?;
    bytes.write_u32::<NativeEndian>(0)?;
    bytes.write_u8(1)?;
    bytes.write_u8(0)?;
    bytes.write_u16::<NativeEndian>(items_in_section.len() as u16)?;

    for item in items_in_section.iter() {
        bytes.write_u32::<NativeEndian>(item.start)?;
        bytes.write_u32::<NativeEndian>(item.end)?;
        bytes.write_f32::<NativeEndian>(item.value)?;
    }

    let (out_bytes, uncompress_buf_size) = if compress {
        let mut compressor = Compressor::new(CompressionLvl::default());
        let max_sz = compressor.zlib_compress_bound(bytes.len());
        let mut compressed_data = vec![0; max_sz];
        let actual_sz = compressor
            .zlib_compress(&bytes, &mut compressed_data)
            .unwrap();
        compressed_data.resize(actual_sz, 0);
        (compressed_data, bytes.len())
    } else {
        (bytes, 0)
    };

    Ok((
        SectionData {
            chrom: chrom_id,
            start,
            end,
            data: out_bytes,
        },
        uncompress_buf_size,
    ))
}

#![allow(non_snake_case)]
#![allow(dead_code)]

use std::io::{self, Seek, SeekFrom};
use std::io::BufWriter;
use std::io::prelude::*;
use std::fs::File;
use std::vec::Vec;

use futures::future::{Future, FutureExt};
use futures::channel::mpsc::{channel, Receiver};
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use futures::task::SpawnExt;

use byteordered::{ByteOrdered, Endianness};

use byteorder::{NativeEndian, WriteBytesExt};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use flate2::read::ZlibDecoder;

use crate::bedgraphparser::ChromGroup;
use crate::chromvalues::{ChromGroups, ChromValues};
use crate::idmap::IdMap;
use crate::tell::Tell;
use crate::tempfilebuffer::{TempFileBuffer, TempFileBufferWriter};
use crate::tempfilewrite::TempFileWrite;

const BIGWIG_MAGIC_LTH: u32 = 0x888F_FC26;
const BIGWIG_MAGIC_HTL: u32 = 0x26FC_8F88;
const BIGBED_MAGIC_LTH: u32 = 0x8789_F2EB;
const BIGBED_MAGIC_HTL: u32 = 0xEBF2_8987;

const CIR_TREE_MAGIC: u32 = 0x2468_ACE0;
const CHROM_TREE_MAGIC: u32 = 0x78CA_8C91;

#[derive(Clone, Debug)]
pub struct BBIHeader {
    pub endianness: Endianness,

    version: u16,
    zoom_levels: u16,
    chromosome_tree_offset: u64,
    full_data_offset: u64,
    full_index_offset: u64,
    field_count: u16,
    defined_field_count: u16,
    auto_sql_offset: u64,
    total_summary_offset: u64,
    uncompress_buf_size: u32,
    reserved: u64,
}

#[derive(Clone, Debug)]
struct ZoomHeader {
    reduction_level: u32,
    data_offset: u64,
    index_offset: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ChromInfo {
    pub(crate) name: String,
    id: u32,
    pub(crate) length: u32,
}

#[derive(Debug)]
pub struct ChromAndSize {
    pub name: String,
    pub length: u32,
}

impl PartialEq for ChromAndSize {
    fn eq(&self, other: &ChromAndSize) -> bool {
        self.name == other.name
    }
}

#[derive(Debug)]
pub struct ChromSize {
    pub name: String,
    pub length: u32,
}

#[derive(Clone, Debug)]
pub struct BigWigInfo {
    pub header: BBIHeader,
    zoom_headers: Vec<ZoomHeader>,
    chrom_info: Vec<ChromInfo>,
}

#[derive(Debug)]
pub(crate) struct Block {
    pub(crate) offset: u64,
    pub(crate) size: u64,
}

#[derive(Debug, Clone)]
pub struct Value {
    pub start: u32,
    pub end: u32,
    pub value: f32,
}

#[derive(Debug, Clone)]
pub struct ValueWithChrom {
    pub chrom: String,
    pub start: u32,
    pub end: u32,
    pub value: f32,
}

#[derive(Debug)]
struct RTreeNodeList<RTreeNode> {
    nodes: Vec<RTreeNode>
}

#[derive(Debug)]
struct RTreeNode {
    start_chrom_idx: u32,
    start_base: u32,
    end_chrom_idx: u32,
    end_base: u32,
    kind: RTreeNodeType,
}

#[derive(Debug)]
enum RTreeNodeType {
    Leaf {
        offset: u64,
        size: u64,
    },
    NonLeaf {
        children: RTreeNodeList<RTreeNode>,
    },
}

#[derive(Debug)]
struct SectionData {
    chrom: u32,
    start: u32,
    end: u32,
    data: Vec<u8>,
}

#[derive(Debug)]
pub struct Section {
    offset: u64,
    size: u64,
    chrom: u32,
    start: u32,
    end: u32,
}

#[derive(Debug)]
pub struct Summary {
    bases_covered: u64,
    min_val: f64,
    max_val: f64,
    sum: f64,
    sum_squares: f64,
}

type TempZoomInfo = (u32 /* resolution */, futures::future::RemoteHandle<std::io::Result<()>> /* Temp file that contains data */, TempFileBuffer, TempFileBuffer /* sections */);
type ZoomInfo = (u32 /* resolution */, File /* Temp file that contains data */, Box<Iterator<Item=Section>> /* sections */);

pub(crate) type ChromGroupRead = (
    Box<Future<Output=io::Result<Summary>> + std::marker::Send + std::marker::Unpin>,
    TempFileBuffer,
    crate::tempfilebuffer::TempFileBuffer,
    Box<Future<Output=io::Result<()>> + std::marker::Send + std::marker::Unpin>,
    Vec<TempZoomInfo>,
    (String, u32)
);

pub trait ChromGroupReadStreamingIterator {
    fn next(&mut self) -> io::Result<Option<ChromGroupRead>>;
}

struct BedGraphSectionItem {
    start: u32,
    end: u32,
    val: f32,
}

#[derive(Debug)]
struct ZoomRecord {
    chrom: u32,
    start: u32,
    end: u32,
    valid_count: u32,
    min_value: f32,
    max_value: f32,
    sum: f32,
    sum_squares: f32,
}

#[derive(Clone)]
pub struct BigWigRead {
    pub path: String,
    pub(crate) info: BigWigInfo,
}

impl BigWigRead {
    pub fn from_file_and_attach(path: String) -> std::io::Result<Self> {
        let fp = File::open(path.clone())?;
        let file = std::io::BufReader::new(fp);
        let info = BigWigRead::read_info(file)?;
        Ok(BigWigRead {
            path,
            info,
        })
    }

    pub fn get_chroms(&self) -> Vec<ChromAndSize> {
        self.info.chrom_info.iter().map(|c| ChromAndSize { name: c.name.clone(), length: c.length }).collect::<Vec<_>>()
    }

    #[allow(clippy::all)]
    pub fn test_read_zoom(&self, chrom_name: &str, start: u32, end: u32) -> std::io::Result<()> {
        let fp = File::open(self.path.clone())?;
        let file = std::io::BufReader::new(fp);

        if self.info.zoom_headers.is_empty() {
            println!("No zooms. Skipping test read.");
            return Ok(())
        }

        let uncompress_buf_size = self.info.header.uncompress_buf_size;
        let index_offset = self.info.zoom_headers[0].index_offset;
        let endianness = self.info.header.endianness;
        let mut file = ByteOrdered::runtime(file, endianness);
        file.seek(SeekFrom::Start(index_offset))?;

        let blocks = self.search_cir_tree(&mut file, chrom_name, start, end)?;

        println!("Number of zoom blocks: {:?}", blocks.len());

        'blocks: for block in blocks {
            println!("Block: {:?}", block);
            file.seek(SeekFrom::Start(block.offset))?;

            let mut raw_data = vec![0u8; block.size as usize];
            file.read_exact(&mut raw_data)?;
            let data = if uncompress_buf_size > 0 {
                let mut uncompressed_block_data = vec![0u8; uncompress_buf_size as usize];
                let mut d = ZlibDecoder::new(&raw_data[..]);
                let _ = d.read(&mut uncompressed_block_data)?;
                uncompressed_block_data
            } else {
                raw_data
            };
            let itemcount = data.len() / (4 * 8);
            assert!(data.len() % (4 * 8) == 0);
            let mut data_mut = ByteOrdered::runtime(&data[..], endianness);
            for _ in 0..itemcount {
                let _chrom_id = data_mut.read_u32()?;
                let _chrom_start = data_mut.read_u32()?;
                let _chrom_end = data_mut.read_u32()?;
                let _valid_count = data_mut.read_u32()?;
                let _min_val = data_mut.read_f32()?;
                let _max_val = data_mut.read_f32()?;
                let _sum_data = data_mut.read_f32()?;
                let _sum_squares = data_mut.read_f32()?;
                println!("First zoom data: {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}", _chrom_id, _chrom_start, _chrom_end, _valid_count, _min_val, _max_val, _sum_data, _sum_squares);
                break 'blocks;
            }
        }

        Ok(())
    }

    fn read_info(file: std::io::BufReader<File>) -> std::io::Result<BigWigInfo> {
        let mut file = ByteOrdered::runtime(file, Endianness::Little);

        let magic = file.read_u32()?;
        println!("Magic {:x?}: ", magic);
        match magic {
            BIGWIG_MAGIC_HTL => {
                file = file.into_opposite();
                true
            },
            BIGWIG_MAGIC_LTH => false,
            _ => return Err(std::io::Error::new(std::io::ErrorKind::Other, "File not a big wig"))
        };

        let version = file.read_u16()?;
        let zoom_levels = file.read_u16()?;
        let chromosome_tree_offset = file.read_u64()?;
        let full_data_offset = file.read_u64()?;
        let full_index_offset = file.read_u64()?;
        let field_count = file.read_u16()?;
        let defined_field_count = file.read_u16()?;
        let auto_sql_offset = file.read_u64()?;
        let total_summary_offset = file.read_u64()?;
        let uncompress_buf_size = file.read_u32()?;
        let reserved = file.read_u64()?;

        let header = BBIHeader {
            endianness: file.endianness(),
            version,
            zoom_levels,
            chromosome_tree_offset,
            full_data_offset,
            full_index_offset,
            field_count,
            defined_field_count,
            auto_sql_offset,
            total_summary_offset,
            uncompress_buf_size,
            reserved,
        };

        println!("Header: {:?}", header);

        let zoom_headers = BigWigRead::read_zoom_headers(&mut file, &header)?;

        // TODO: could instead store this as an Option and only read when needed
        file.seek(SeekFrom::Start(header.chromosome_tree_offset))?;
        let magic = file.read_u32()?;
        let _block_size = file.read_u32()?;
        let key_size = file.read_u32()?;
        let val_size = file.read_u32()?;
        let item_count = file.read_u64()?;
        let _reserved = file.read_u64()?;
        if magic != CHROM_TREE_MAGIC {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid file format: CHROM_TREE_MAGIC does not match."))
        }
        //println!("{:x?} {:?} {:?} {:?} {:?} {:?}", magic, _block_size, key_size, val_size, item_count, _reserved);
        assert_eq!(val_size, 8u32); 

        let mut chrom_info = Vec::with_capacity(item_count as usize);
        BigWigRead::read_chrom_tree_block(&mut file, &mut chrom_info, key_size)?;

        let info = BigWigInfo {
            header,
            zoom_headers,
            chrom_info,
        };

        println!("Info read successfully.");
        Ok(info)
    }

    fn read_zoom_headers(file: &mut ByteOrdered<std::io::BufReader<File>, Endianness>, header: &BBIHeader) -> std::io::Result<Vec<ZoomHeader>> {
        let mut zoom_headers = vec![];
        for _ in 0..header.zoom_levels {
            let reduction_level = file.read_u32()?;
            let _reserved = file.read_u32()?;
            let data_offset = file.read_u64()?;
            let index_offset = file.read_u64()?;

            //println!("Zoom header: reductionLevel: {:?} Reserved: {:?} Data offset: {:?} Index offset: {:?}", reduction_level, _reserved, data_offset, index_offset);

            zoom_headers.push(ZoomHeader {
                reduction_level,
                data_offset,
                index_offset,
            });
        }

        Ok(zoom_headers)
    }

    fn read_chrom_tree_block(f: &mut ByteOrdered<std::io::BufReader<File>, Endianness>, chroms: &mut Vec<ChromInfo>, key_size: u32) -> std::io::Result<()> {
        let isleaf = f.read_u8()?;
        let _reserved = f.read_u8()?;
        let count = f.read_u16()?;

        if isleaf == 1 {
            for _ in 0..count {
                let mut key_bytes = vec![0u8; key_size as usize];
                f.read_exact(&mut key_bytes)?;
                let key_string = match String::from_utf8(key_bytes) {
                    Ok(s) => s.trim_matches(char::from(0)).to_owned(),
                    Err(_) => return Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid file format: Invalid utf-8 string.")),
                };
                let chrom_id = f.read_u32()?;
                let chrom_size = f.read_u32()?;
                chroms.push(ChromInfo {
                    name: key_string,
                    id: chrom_id,
                    length: chrom_size,
                });
            }
        } else {
            let mut current_position: u64;
            for _ in 0..count {
                let mut key_bytes = vec![0u8; key_size as usize];
                f.read_exact(&mut key_bytes)?;
                // TODO: could add specific find here by comparing key string
                let child_offset = f.read_u64()?;
                current_position = f.seek(SeekFrom::Current(0))?;
                f.seek(SeekFrom::Start(child_offset))?;
                BigWigRead::read_chrom_tree_block(f, chroms, key_size)?;
                f.seek(SeekFrom::Start(current_position))?;
            }
        }
        Ok(())
    }

    #[inline]
    fn compare_position(chrom1: u32, chrom1_base: u32, chrom2: u32, chrom2_base: u32) -> i8 {
        if chrom1 < chrom2 {
            -1
        } else if chrom1 > chrom2 {
            1
        } else if chrom1_base < chrom2_base {
            -1
        } else if chrom1_base > chrom2_base {
            1
        } else {
            0
        }
    }

    fn overlaps(chromq: u32, chromq_start: u32, chromq_end: u32, chromb1: u32, chromb1_start: u32, chromb2: u32, chromb2_end: u32) -> bool {
        BigWigRead::compare_position(chromq, chromq_start, chromb2, chromb2_end) <= 0 && BigWigRead::compare_position(chromq, chromq_end, chromb1, chromb1_start) >= 0
    }

    fn search_overlapping_blocks(mut file: &mut ByteOrdered<std::io::BufReader<File>, Endianness>, chrom_ix: u32, start: u32, end: u32, mut blocks: &mut Vec<Block>) -> std::io::Result<()> {
        //println!("Searching for overlapping blocks at {:?}. Searching {:?}:{:?}-{:?}", self.current_file_offset()?, chrom_ix, start, end);

        let isleaf: u8 = file.read_u8()?;
        assert!(isleaf == 1 || isleaf == 0, "Unexpected isleaf: {}", isleaf);
        let _reserved = file.read_u8()?;
        let count: u16 = file.read_u16()?;
        //println!("Index: {:?} {:?} {:?}", isleaf, _reserved, count);

        let mut childblocks: Vec<u64> = vec![];
        for _ in 0..count {
            let start_chrom_ix = file.read_u32()?;
            let start_base = file.read_u32()?;
            let end_chrom_ix = file.read_u32()?;
            let end_base = file.read_u32()?;
            if isleaf == 1 {
                let data_offset = file.read_u64()?;
                let data_size = file.read_u64()?;
                if !BigWigRead::overlaps(chrom_ix, start, end, start_chrom_ix, start_base, end_chrom_ix, end_base) {
                    continue;
                }
                //println!("Overlaps (leaf): {:?}:{:?}-{:?} with {:?}:{:?}-{:?}:{:?} {:?} {:?}", chrom_ix, start, end, start_chrom_ix, start_base, end_chrom_ix, end_base, data_offset, data_size);
                blocks.push(Block {
                    offset: data_offset,
                    size: data_size,
                })
            } else {
                let data_offset = file.read_u64()?;
                if !BigWigRead::overlaps(chrom_ix, start, end, start_chrom_ix, start_base, end_chrom_ix, end_base) {
                    continue;
                }
                //println!("Overlaps (non-leaf): {:?}:{:?}-{:?} with {:?}:{:?}-{:?}:{:?} {:?}", chrom_ix, start, end, start_chrom_ix, start_base, end_chrom_ix, end_base, data_offset);
                childblocks.push(data_offset);
            }
        }
        for childblock in childblocks {
            //println!("Seeking to {:?}", childblock);
            file.seek(SeekFrom::Start(childblock))?;
            BigWigRead::search_overlapping_blocks(&mut file, chrom_ix, start, end, &mut blocks)?;
        }
        Ok(())
    }

    fn search_cir_tree(&self, mut file: &mut ByteOrdered<std::io::BufReader<File>, Endianness>, chrom_name: &str, start: u32, end: u32) -> std::io::Result<Vec<Block>> {
        let chrom_ix = {
            let chrom_info = &self.info.chrom_info;
            let chrom = chrom_info.iter().find(|&x| x.name == chrom_name);
            //println!("Chrom: {:?}", chrom);
            match chrom {
                Some(c) => c.id,
                None => return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("{} not found.", chrom_name)))
            }
        };

        let magic = file.read_u32()?;
        if magic != CIR_TREE_MAGIC {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid file format: CIR_TREE_MAGIC does not match."));
        }
        let _blocksize = file.read_u32()?;
        let _item_count = file.read_u64()?;
        let _start_chrom_idx = file.read_u32()?;
        let _start_base = file.read_u32()?;
        let _end_chrom_idx = file.read_u32()?;
        let _end_base = file.read_u32()?;
        let _end_file_offset = file.read_u64()?;
        let _item_per_slot = file.read_u32()?;
        let _reserved = file.read_u32()?;

        // TODO: could do some optimization here to check if our interval overlaps with any data

        //println!("cirTree header:\n bs: {:?}\n ic: {:?}\n sci: {:?}\n sb: {:?}\n eci: {:?}\n eb: {:?}\n efo: {:?}\n ips: {:?}\n r: {:?}", _blocksize, _item_count, _start_chrom_idx, _start_base, _end_chrom_idx, _end_base, _end_file_offset, _item_per_slot, _reserved);
        let mut blocks: Vec<Block> = vec![];
        BigWigRead::search_overlapping_blocks(&mut file, chrom_ix, start, end, &mut blocks)?;
        //println!("overlapping_blocks: {:?}", blocks);
        Ok(blocks)
    }

    pub(crate) fn get_overlapping_blocks(&self, chrom_name: &str, start: u32, end: u32) -> std::io::Result<Vec<Block>> {
        let endianness = self.info.header.endianness;
        let fp = File::open(self.path.clone())?;
        let mut file = ByteOrdered::runtime(std::io::BufReader::new(fp), endianness);

        let full_index_offset = self.info.header.full_index_offset;
        file.seek(SeekFrom::Start(full_index_offset))?;

        self.search_cir_tree(&mut file, chrom_name, start, end)
    }

    /// This assumes that the file is currently at the block's start
    pub(crate) fn get_block_values(&self, file: &mut ByteOrdered<std::io::BufReader<File>, Endianness>, block: &Block) -> std::io::Result<impl Iterator<Item=Value>> {
        let endianness = self.info.header.endianness;
        let uncompress_buf_size: usize = self.info.header.uncompress_buf_size as usize;
        let mut values: Vec<Value> = Vec::new();

        let mut raw_data = vec![0u8; block.size as usize];
        file.read_exact(&mut raw_data)?;
        let block_data: Vec<u8> = if uncompress_buf_size > 0 {
            let mut uncompressed_block_data = vec![0u8; uncompress_buf_size];
            let mut d = ZlibDecoder::new(&raw_data[..]);
            let _ = d.read(&mut uncompressed_block_data)?;
            uncompressed_block_data
        } else {
            raw_data
        };

        let mut block_data_mut = ByteOrdered::runtime(&block_data[..], endianness);
        let _chrom_id = block_data_mut.read_u32()?;
        let chrom_start = block_data_mut.read_u32()?;
        let _chrom_end = block_data_mut.read_u32()?;
        let item_step = block_data_mut.read_u32()?;
        let item_span = block_data_mut.read_u32()?;
        let section_type = block_data_mut.read_u8()?;
        let _reserved = block_data_mut.read_u8()?;
        let item_count = block_data_mut.read_u16()?;

        let mut start = chrom_start;
        for _ in 0..item_count {
            match section_type {
                1 => {
                    // bedgraph
                    let chrom_start = block_data_mut.read_u32()?;
                    let chrom_end = block_data_mut.read_u32()?;
                    let value = block_data_mut.read_f32()?;
                    values.push(Value {
                        start: chrom_start,
                        end: chrom_end,
                        value,
                    });
                },
                2 => {
                    // variable step
                    let chrom_start = block_data_mut.read_u32()?;
                    let chrom_end = chrom_start + item_span;
                    let value = block_data_mut.read_f32()?;
                    values.push(Value {
                        start: chrom_start,
                        end: chrom_end,
                        value,
                    });
                },
                3 => {
                    // fixed step
                    let chrom_start = start;
                    start += item_step;
                    let chrom_end = chrom_start + item_span;
                    let value = block_data_mut.read_f32()?;
                    values.push(Value {
                        start: chrom_start,
                        end: chrom_end,
                        value,
                    });
                },
                _ => return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("Unknown bigwig section type: {}", section_type)))
            }
        }

        Ok(values.into_iter())
    }

    pub fn get_interval<'a>(&'a self, chrom_name: &str, start: u32, end: u32) -> std::io::Result<impl Iterator<Item=Value> + std::marker::Send + 'a> {
        let blocks = self.get_overlapping_blocks(chrom_name, start, end)?;

        let endianness = self.info.header.endianness;
        let fp = File::open(self.path.clone())?;
        let mut file = ByteOrdered::runtime(std::io::BufReader::new(fp), endianness);

        if blocks.len() > 0 {
            file.seek(SeekFrom::Start(blocks[0].offset))?;
        }
        let mut iter = blocks.into_iter().peekable();
        
        let block_iter = std::iter::from_fn(move || {
            let next = iter.next();
            let peek = iter.peek();
            let next_offset = match peek {
                None => None,
                Some(peek) => Some(peek.offset),
            };
            match next {
                None => None,
                Some(next) => Some((next, next_offset))
            }
        });
        let vals_iter = block_iter.flat_map(move |(block, next_offset)| {
            // TODO: Could minimize this by chunking block reads
            let vals = self.get_block_values(&mut file, &block).unwrap();
            match next_offset {
                None => (),
                Some(next_offset) => {
                    if next_offset != block.offset + block.size {
                        file.seek(SeekFrom::Start(next_offset)).unwrap();
                    }
                }
            }
            vals
        });

        Ok(vals_iter)
    }
}

#[derive(Clone)]
pub struct BigWigWriteOptions {
    pub compress: bool,
    pub items_per_slot: u32,
    pub block_size: u32,
}

pub struct BigWigWrite {
    pub path: String,
    pub options: BigWigWriteOptions,
}

impl BigWigWrite {
    pub fn create_file(path: String) -> std::io::Result<Self> {
        Ok(BigWigWrite {
            path,
            options: BigWigWriteOptions {
                compress: true,
                items_per_slot: 1024,
                block_size: 256,
            }
        })
    }

    const MAX_ZOOM_LEVELS: usize = 10;

    pub fn write<V: 'static>(&self, chrom_sizes: std::collections::HashMap<String, u32>, vals: V) -> io::Result<()> where V : ChromGroups<ChromGroup> + std::marker::Send {        
        struct ChromGroupReadStreamingIteratorImpl<C: ChromGroups<ChromGroup>> {
            chrom_groups: C,
            last_chrom: Option<String>,
            chrom_ids: IdMap<String>,
            pool: futures::executor::ThreadPool,
            options: BigWigWriteOptions,
        }

        impl<C: ChromGroups<ChromGroup>> ChromGroupReadStreamingIterator for ChromGroupReadStreamingIteratorImpl<C> {
            fn next(&mut self) -> io::Result<Option<ChromGroupRead>> {
                match self.chrom_groups.next()? {
                    Some((chrom, group)) => {
                        let last = self.last_chrom.replace(chrom.clone());
                        if let Some(c) = last {
                            // TODO: test this correctly fails
                            // TODO: change these to not panic
                            assert!(c < chrom, "Input bedGraph not sorted by chromosome. Sort with `sort -k1,1 -k2,2n`.");
                        }
                        let chrom_id = self.chrom_ids.get_id(chrom.clone());
                        Ok(Some(BigWigWrite::read_group(chrom, chrom_id, group, self.pool.clone(), self.options.clone()).unwrap()))
                    },
                    None => Ok(None),
                }
            }
        }

        let group_iter = ChromGroupReadStreamingIteratorImpl {
            chrom_groups: vals,
            last_chrom: None,
            chrom_ids: IdMap::new(),
            pool: futures::executor::ThreadPoolBuilder::new().pool_size(4).create().expect("Unable to create thread pool."),
            options: self.options.clone(),
        };
        self.write_groups(chrom_sizes, group_iter)
    }

    pub fn write_groups<V: 'static>(&self, chrom_sizes: std::collections::HashMap<String, u32>, vals: V) -> std::io::Result<()> where V : ChromGroupReadStreamingIterator + std::marker::Send {
        let fp = File::create(self.path.clone())?;
        let mut file = BufWriter::new(fp);

        BigWigWrite::write_blank_headers(&mut file)?;

        let total_summary_offset = file.tell()?;
        file.write_all(&[0; 40])?;

        let full_data_offset = file.tell()?;

        {
            // Total items
            // Unless we know the vals ahead of time, we can't estimate total sections ahead of time.
            // Even then simply doing "(vals.len() as u32 + ITEMS_PER_SLOT - 1) / ITEMS_PER_SLOT"
            // underestimates because sections are split by chrom too, not just size.
            // Skip for now, and come back when we write real header + summary.
            file.write_u32::<NativeEndian>(0)?;
        }

        let pre_data = file.tell()?;
        let (chrom_summary_future, raw_sections_iter) = self.write_vals(vals, file)?;
        let sections_iter = raw_sections_iter.map(|mut section| {
            section.offset += pre_data;
            section
        });
        let (chrom_ids, summary, mut file, zoom_infos) = futures::executor::block_on(chrom_summary_future);
        let (nodes, levels, total_sections) = BigWigWrite::get_rtreeindex(sections_iter, &self.options);
        let data_size = file.tell()? - pre_data;
        println!("Data size: {:?}", data_size);
        println!("Sections: {:?}", total_sections);
        println!("Summary: {:?}", summary);
        println!("Zooms: {:?}", zoom_infos.len());

        // Since the chrom tree is read before the index, we put this before the full data index
        // Therefore, there is a higher likelihood that the udc file will only need one read for chrom tree + full data index
        // Putting the chrom tree before the data also has a higher likelihood of being included with the beginning headers,
        //  but requires us to know all the data ahead of time (when writing)
        let chrom_index_start = file.tell()?;
        BigWigWrite::write_chrom_tree(&mut file, chrom_sizes, &chrom_ids.get_map())?;

        let index_start = file.tell()?;
        BigWigWrite::write_rtreeindex(&mut file, nodes, levels, total_sections, &self.options)?;

        let mut zoom_entries: Vec<ZoomHeader> = vec![];
        BigWigWrite::write_zooms(&mut file, zoom_infos, &mut zoom_entries, data_size, &self.options)?;

        //println!("Zoom entries: {:?}", zoom_entries);
        let num_zooms = zoom_entries.len() as u16;

        // We *could* actually check the the real max size, but let's just assume at it's as large as the largest possible value
        // In most cases, I think this is the true max size (unless there is only one section and its less than ITEMS_PER_SLOT in size)
        let uncompress_buf_size = if self.options.compress {
            self.options.items_per_slot * (1 + 1 + 2 + 4 + 4 + 4 + 4 + 8 + 8)
        } else {
            0
        };

        file.seek(SeekFrom::Start(0))?;
        file.write_u32::<NativeEndian>(BIGWIG_MAGIC_LTH)?;
        file.write_u16::<NativeEndian>(4)?;
        file.write_u16::<NativeEndian>(num_zooms)?;
        file.write_u64::<NativeEndian>(chrom_index_start)?;
        file.write_u64::<NativeEndian>(full_data_offset)?;
        file.write_u64::<NativeEndian>(index_start)?;
        file.write_u16::<NativeEndian>(0)?;
        file.write_u16::<NativeEndian>(0)?;
        file.write_u64::<NativeEndian>(0)?;
        file.write_u64::<NativeEndian>(total_summary_offset)?;
        file.write_u32::<NativeEndian>(uncompress_buf_size)?;
        file.write_u64::<NativeEndian>(0)?;

        assert!(file.seek(SeekFrom::Current(0))? == 64);

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


        file.write_u32::<NativeEndian>(total_sections as u32)?;
        file.seek(SeekFrom::End(0))?;
        file.write_u32::<NativeEndian>(BIGWIG_MAGIC_LTH)?;

        Ok(())
    }

    fn write_blank_headers(file: &mut BufWriter<File>) -> std::io::Result<()> {
        file.seek(SeekFrom::Start(0))?;
        // Common header
        file.write_all(&[0; 64])?;
        // Zoom levels
        file.write_all(&[0; BigWigWrite::MAX_ZOOM_LEVELS * 24])?;

        Ok(())
    }

    fn write_chrom_tree(file: &mut BufWriter<File>, chrom_sizes: std::collections::HashMap<String, u32>, chrom_ids: &std::collections::HashMap<String, u32>) -> std::io::Result<()> {
        let mut chroms: Vec<&String> = chrom_ids.keys().collect();
        chroms.sort();
        //println!("Used chroms {:?}", chroms);

        file.write_u32::<NativeEndian>(CHROM_TREE_MAGIC)?;
        let item_count = chroms.len() as u64;
        // TODO: for now, always just use the length of chroms (if less than 256). This means we don't have to implement writing non-leaf nodes for now...
        // TODO: make this configurable
        let block_size = std::cmp::max(256, item_count) as u32;
        file.write_u32::<NativeEndian>(block_size)?;
        let max_bytes = chroms.iter().map(|a| a.len() as u32).fold(0, u32::max);
        file.write_u32::<NativeEndian>(max_bytes)?;
        file.write_u32::<NativeEndian>(8)?; // size of Id (u32) + Size (u32)
        file.write_u64::<NativeEndian>(item_count)?;
        file.write_u64::<NativeEndian>(0)?; // Reserved

        // Assuming this is all one block right now
        // TODO: add non-leaf nodes and split blocks
        file.write_u8(1)?;
        file.write_u8(0)?;
        file.write_u16::<NativeEndian>(item_count as u16)?;
        for chrom in chroms {
            let key_bytes = &mut vec![0u8; max_bytes as usize];
            let chrom_bytes = chrom.as_bytes();
            key_bytes[..chrom_bytes.len()].copy_from_slice(chrom_bytes);
            file.write_all(key_bytes)?;
            let id = *chrom_ids.get(chrom).unwrap();
            file.write_u32::<NativeEndian>(id)?;
            let length = chrom_sizes.get(&chrom[..]);
            match length {
                None => panic!("Expected length for chrom: {}", chrom),
                Some(l) => {
                    file.write_u32::<NativeEndian>(*l)?;
                }
            }
        }
        Ok(())
    }

    fn create_section_iter(mut bufreader: ByteOrdered<std::io::BufReader<crate::tempfilewrite::TempFileWriteReader>, Endianness>) -> Box<Iterator<Item=Section>> {
        let section_iter = std::iter::from_fn(move || {
            let next_read = bufreader.read_u32();
            if let Err(_) = next_read {
                return None;
            }
            let chrom = next_read.unwrap();
            let start = bufreader.read_u32().unwrap();
            let end = bufreader.read_u32().unwrap();
            let offset = bufreader.read_u64().unwrap();
            let size = bufreader.read_u64().unwrap();
            Some(Section {
                chrom,
                start,
                end,
                offset,
                size,
            })
        });
        let res: Box<Iterator<Item=Section> + std::marker::Send> = Box::new(section_iter);
        res
    }

    async fn write_section(compress: bool, items_in_section: Vec<BedGraphSectionItem>, chromId: u32) -> std::io::Result<SectionData> {
        let mut bytes: Vec<u8> = vec![];

        let start = items_in_section[0].start;
        let end = items_in_section[items_in_section.len() - 1].end;
        bytes.write_u32::<NativeEndian>(chromId)?;
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
            bytes.write_f32::<NativeEndian>(item.val)?;   
        }

        let out_bytes = if compress {
            let mut e = ZlibEncoder::new(Vec::with_capacity(bytes.len()), Compression::default());
            e.write_all(&bytes)?;
            e.finish()?
        } else {
            bytes
        };

        Ok(SectionData {
            chrom: chromId,
            start,
            end,
            data: out_bytes,
        })
    }

    async fn write_zoom_section(compress: bool, items_in_section: Vec<ZoomRecord>) -> std::io::Result<SectionData> {
        let mut bytes: Vec<u8> = vec![];

        let start = items_in_section[0].start;
        let end = items_in_section[items_in_section.len() - 1].end;

        let chrom = items_in_section[0].chrom;
        for item in items_in_section.iter() {
            bytes.write_u32::<NativeEndian>(item.chrom)?;
            bytes.write_u32::<NativeEndian>(item.start)?;
            bytes.write_u32::<NativeEndian>(item.end)?;
            bytes.write_u32::<NativeEndian>(item.valid_count)?;
            bytes.write_f32::<NativeEndian>(item.min_value)?;
            bytes.write_f32::<NativeEndian>(item.max_value)?;
            bytes.write_f32::<NativeEndian>(item.sum)?;
            bytes.write_f32::<NativeEndian>(item.sum_squares)?; 
        }

        let out_bytes = if compress {
            let mut e = ZlibEncoder::new(Vec::with_capacity(bytes.len()), Compression::default());
            e.write_all(&bytes)?;
            e.finish()?
        } else {
            bytes
        };

        Ok(SectionData {
            chrom,
            start,
            end,
            data: out_bytes,
        })
    }

    pub(crate) fn read_group<I: 'static>(chrom: String, chromId: u32, mut group: I, mut pool: futures::executor::ThreadPool, options: BigWigWriteOptions)
        -> io::Result<ChromGroupRead>
        where I: ChromValues + std::marker::Send {
        let cloned_chrom = chrom.clone();

        let zoom_sizes: Vec<u32> = vec![10, 40, 160, 640, 2_560, 10_240, 40_960, 163_840, 655_360, 2_621_440, 10_485_760];
        let num_zooms = zoom_sizes.len();

        let (mut ftx, frx) = channel::<_>(100);

        async fn create_do_write<W: Write>(mut file: BufWriter<W>, mut bufwriter: ByteOrdered<BufWriter<TempFileBufferWriter>, Endianness>, mut frx: Receiver<impl futures::Future<Output=std::io::Result<SectionData>>>) -> std::io::Result<()> {
            let mut current_offset = 0;
            while let Some(section_raw) = frx.next().await {
                let section: SectionData = section_raw.await?;
                let size = section.data.len() as u64;
                file.write_all(&section.data)?;
                bufwriter.write_u32(section.chrom)?;
                bufwriter.write_u32(section.start)?;
                bufwriter.write_u32(section.end)?;
                bufwriter.write_u64(current_offset)?;
                bufwriter.write_u64(size)?;
                current_offset += size;
            }
            Ok(())
        };

        let (sections_future, buf, section_but) = {
            let (section_but, writer) = TempFileBuffer::new()?;
            let bufwriter = ByteOrdered::runtime(BufWriter::new(writer), Endianness::native());

            let (buf, write) = TempFileBuffer::new()?;
            let file = BufWriter::new(write);

            let sections_future = create_do_write(file, bufwriter, frx);
            (sections_future, buf, section_but)
        };

        let process_zooms = move |zoom_channel: Receiver<_>, size: u32| -> std::io::Result<_> {
            let (section_but, writer) = TempFileBuffer::new()?;
            let bufwriter = ByteOrdered::runtime(std::io::BufWriter::new(writer), Endianness::native());

            let (buf, write) = crate::tempfilebuffer::TempFileBuffer::new()?;
            let file = BufWriter::new(write);

            let file_future = create_do_write(file, bufwriter, zoom_channel);

            Ok((size, file_future, buf, section_but))
        };

        let (zooms_futures, mut zooms_channels): (Vec<_>, Vec<_>) = zoom_sizes.iter().map(|size| {
            let (ftx, frx) = channel::<_>(100);
            let f = process_zooms(frx, *size);
            (f, ftx)
        }).unzip();

        struct LiveZoomInfo {
            chrom: String,
            start: u32,
            end: u32,
            valid_count: u32,
            min_value: f32,
            max_value: f32,
            sum: f32,
            sum_squares: f32,
        }

        struct ZoomItem {
            live_info: Option<LiveZoomInfo>,
            records: Vec<ZoomRecord>,
        }
        struct BedGraphSection {
            items: Vec<BedGraphSectionItem>,
            zoom_items: Vec<ZoomItem>
        }
        
        let read_file = async move || -> std::io::Result<Summary> {
            let mut summary: Option<Summary> = None;

            let mut state_val = BedGraphSection {
                items: Vec::with_capacity(options.items_per_slot as usize),
                zoom_items: (0..num_zooms).map(|_| ZoomItem {
                    live_info: None,
                    records: Vec::with_capacity(options.items_per_slot as usize)
                }).collect(),
            };
            while let Some(current_val) = group.next()? {
                // TODO: test this correctly fails
                // TODO: change these to not panic
                assert!(current_val.start <= current_val.end);

                for (i, mut zoom_item) in state_val.zoom_items.iter_mut().enumerate() {
                    let mut add_start = current_val.start;
                    loop {
                        if add_start >= current_val.end {
                            break
                        }
                        match &mut zoom_item.live_info {
                            None => {
                                zoom_item.live_info = Some(LiveZoomInfo {
                                    chrom: chrom.clone(),
                                    start: add_start,
                                    end: add_start,
                                    valid_count: 0,
                                    min_value: current_val.value,
                                    max_value: current_val.value,
                                    sum: 0.0,
                                    sum_squares: 0.0,
                                });
                            },
                            Some(zoom2) => {
                                let next_end = zoom2.start + zoom_sizes[i];
                                // End of bases that we could add
                                let add_end = std::cmp::min(next_end, current_val.end);
                                // If the last zoom ends before this value starts, we don't add anything
                                if add_end >= add_start {
                                    let added_bases = add_end - add_start;                                
                                    zoom2.end = add_end;
                                    zoom2.valid_count += added_bases;
                                    zoom2.min_value = zoom2.min_value.min(current_val.value);
                                    zoom2.max_value = zoom2.max_value.max(current_val.value);
                                    zoom2.sum += added_bases as f32 * current_val.value;
                                    zoom2.sum_squares += added_bases as f32 * current_val.value * current_val.value;
                                }
                                // If we made it to the end of the zoom (whether it was because the zoom ended before this value started,
                                // or we added to the end of the zoom), then write this zooms to the current section
                                if add_end == next_end {
                                    zoom_item.records.push(ZoomRecord {
                                        chrom: chromId,
                                        start: zoom2.start,
                                        end: zoom2.end,
                                        valid_count: zoom2.valid_count,
                                        min_value: zoom2.min_value,
                                        max_value: zoom2.max_value,
                                        sum: zoom2.sum,
                                        sum_squares: zoom2.sum_squares,
                                    });
                                    zoom_item.live_info = None;
                                }
                                // Set where we would start for next time
                                add_start = std::cmp::max(add_end, current_val.start);
                                // Write section if full
                                debug_assert!(zoom_item.records.len() <= options.items_per_slot as usize);
                                if zoom_item.records.len() == options.items_per_slot as usize {
                                    let items = std::mem::replace(&mut zoom_item.records, vec![]);
                                    let handle = pool.spawn_with_handle(BigWigWrite::write_zoom_section(options.compress, items)).expect("Couldn't spawn.");
                                    zooms_channels[i].send(handle.boxed()).await.expect("Couln't send");
                                }
                            }
                        }
                    }
                }
                state_val.items.push(BedGraphSectionItem {
                    start: current_val.start,
                    end: current_val.end,
                    val: current_val.value,
                });
                if state_val.items.len() >= options.items_per_slot as usize {
                    let items = std::mem::replace(&mut state_val.items, vec![]);
                    let handle = pool.spawn_with_handle(BigWigWrite::write_section(options.compress, items, chromId)).expect("Couldn't spawn.");
                    ftx.send(handle.boxed()).await.expect("Couldn't send");
                }

                match &mut summary {
                    None => {
                        summary = Some(Summary {
                            bases_covered: u64::from(current_val.end - current_val.start),
                            min_val: f64::from(current_val.value),
                            max_val: f64::from(current_val.value),
                            sum: f64::from(current_val.end - current_val.start) * f64::from(current_val.value),
                            sum_squares: f64::from(current_val.end - current_val.start) * f64::from(current_val.value * current_val.value),
                        })
                    },
                    Some(summary) => {
                        summary.bases_covered += u64::from(current_val.end - current_val.start);
                        summary.min_val = summary.min_val.min(f64::from(current_val.value));
                        summary.max_val = summary.max_val.max(f64::from(current_val.value));
                        summary.sum += f64::from(current_val.end - current_val.start) * f64::from(current_val.value);
                        summary.sum_squares += f64::from(current_val.end - current_val.start) * f64::from(current_val.value * current_val.value);
                    }
                }

                match group.peek() {
                    None => (),
                    Some(next_val) => {
                        assert!(
                            current_val.end <= next_val.start,
                            "Input bedGraph has overlapping values on chromosome {} at {}-{} and {}-{}",
                            chrom,
                            current_val.start,
                            current_val.end,
                            next_val.start,
                            next_val.end,
                        );
                    }
                }
            }

            let lastchrom = chrom.clone();
            if !state_val.items.is_empty() {
                let handle = pool.spawn_with_handle(BigWigWrite::write_section(options.compress, state_val.items, chromId)).expect("Couldn't spawn.");
                ftx.send(handle.boxed()).await.expect("Couldn't send");
            }

            for (i, mut zoom_item) in state_val.zoom_items.into_iter().enumerate() {
                if let Some(zoom2) = zoom_item.live_info {
                    assert!(lastchrom == zoom2.chrom);
                    zoom_item.records.push(ZoomRecord {
                        chrom: chromId,
                        start: zoom2.start,
                        end: zoom2.end,
                        valid_count: zoom2.valid_count,
                        min_value: zoom2.min_value,
                        max_value: zoom2.max_value,
                        sum: zoom2.sum,
                        sum_squares: zoom2.sum_squares,
                    });
                    zoom_item.live_info = None;
                }
                if !zoom_item.records.is_empty() {
                    let items = zoom_item.records;
                    let handle = pool.spawn_with_handle(BigWigWrite::write_zoom_section(options.compress, items)).expect("Couldn't spawn.");
                    zooms_channels[i].send(handle.boxed()).await.expect("Couln't send");
                }
            }

            let summary_complete = match summary {
                None => Summary {
                    bases_covered: 0,
                    min_val: 0.0,
                    max_val: 0.0,
                    sum: 0.0,
                    sum_squares: 0.0,
                },
                Some(summary) => summary,
            };
            Ok(summary_complete)
        };

        let f = read_file();

        let (zoom_infos, zoom_remotes): (Vec<_>, Vec<_>) = zooms_futures.into_iter().map(|handle| {
            let (size, file_future, buf, section_iter) = handle.unwrap();
            let (remote, handle) = file_future.remote_handle();
            ((size, handle, buf, section_iter), remote)
        }).unzip();

        std::thread::spawn(move || {
            let mut pool = futures::executor::LocalPool::new();
            for zoom_remote in zoom_remotes {
                pool.spawner().spawn(zoom_remote).expect("Couldn't spawn future.");
            }
            pool.run()
        });

        let (sections_remote, sections_handle) = sections_future.remote_handle();
        std::thread::spawn(move || {
            futures::executor::block_on(sections_remote);
        });

        let (f_remote, f_handle) = f.remote_handle();
        std::thread::spawn(move || {
            futures::executor::block_on(f_remote);
        });
        Ok((Box::new(f_handle), section_but, buf, Box::new(sections_handle), zoom_infos, (cloned_chrom, chromId)))    
    }

    fn write_vals<V: 'static>(
        &self,
        mut vals_iter: V,
        file: BufWriter<File>
    ) -> std::io::Result<(
        impl futures::Future<Output=(IdMap<String>, Summary, BufWriter<File>, Vec<ZoomInfo>)>,
        impl Iterator<Item=Section>,
        )> where V : ChromGroupReadStreamingIterator + std::marker::Send {

        let zoom_sizes: Vec<u32> = vec![10, 40, 160, 640, 2_560, 10_240, 40_960, 163_840, 655_360, 2_621_440, 10_485_760];

        let (mut writer, reader) = TempFileWrite::new()?;
        let bo_reader = ByteOrdered::runtime(std::io::BufReader::new(reader), Endianness::native());

        let read_file = async move || -> std::io::Result<(IdMap<String>, Summary, BufWriter<File>, Vec<ZoomInfo>)> {
            let mut summary: Option<Summary> = None;


            let mut zooms: Vec<_> = zoom_sizes.iter().map(|size| {
                let (writer, reader) = TempFileWrite::new(). unwrap();
                let bo_reader = ByteOrdered::runtime(std::io::BufReader::new(reader), Endianness::native());

                let (buf, write) = crate::tempfilebuffer::TempFileBuffer::new().unwrap();
                (size, writer, bo_reader, buf, write)
            }).collect();

            let mut chrom_ids = IdMap::new();
            let mut raw_file = file.into_inner().unwrap();
            while let Some((summary_future, sections_idx_file, mut sections_buf, sections_future, zoom_infos, (chrom, chrom_id))) = vals_iter.next()? {
                let real_id = chrom_ids.get_id(chrom);
                assert_eq!(real_id, chrom_id);
                sections_buf.switch(raw_file)?;

                let chrom_summary = summary_future.await?;
                sections_future.await?;
                sections_idx_file.expect_closed_write(&mut writer)?;
                raw_file = sections_buf.await_file();

                for (i, (_size, future, buf, zoom_sections_idx_file)) in zoom_infos.into_iter().enumerate() {
                    let zoom = &mut zooms[i];
                    // Await this future for its Result
                    future.await?;
                    zoom_sections_idx_file.expect_closed_write(&mut zoom.1)?;
                    buf.expect_closed_write(&mut zoom.4)?;
                }

                match &mut summary {
                    None => {
                        summary = Some(chrom_summary)
                    },
                    Some(summary) => {
                        summary.bases_covered += chrom_summary.bases_covered;
                        summary.min_val = summary.min_val.min(chrom_summary.min_val);
                        summary.max_val = summary.max_val.max(chrom_summary.max_val);
                        summary.sum += chrom_summary.sum;
                        summary.sum_squares += chrom_summary.sum_squares;
                    }
                }
            }

            let summary_complete = match summary {
                None => Summary {
                    bases_covered: 0,
                    min_val: 0.0,
                    max_val: 0.0,
                    sum: 0.0,
                    sum_squares: 0.0,
                },
                Some(summary) => summary,
            };

            let zoom_infos: Vec<_> = zooms.into_iter().map(|zoom| {
                drop(zoom.4);
                (*zoom.0, zoom.3.await_raw(), BigWigWrite::create_section_iter(zoom.2))
            }).collect();
            Ok((chrom_ids, summary_complete, BufWriter::new(raw_file), zoom_infos))
        };

        let f = read_file();

        Ok((f.map(Result::unwrap), BigWigWrite::create_section_iter(bo_reader)))
    }

    fn write_zooms<'a>(mut file: &'a mut BufWriter<File>, zooms: Vec<ZoomInfo>, zoom_entries: &'a mut Vec<ZoomHeader>, data_size: u64, options: &BigWigWriteOptions) -> std::io::Result<()> {
        let mut zoom_count = 0;
        for zoom in zooms {
            let mut zoom_file = zoom.1;
            let zoom_size = zoom_file.seek(SeekFrom::End(0))?;
            if zoom_size > (data_size / 2) {
                //println!("Skipping zoom {:?} because it's size ({:?}) is greater than the data_size/2 ({:?})", zoom.0, zoom.3, data_size/2);
                continue;
            }
            let zoom_data_offset = file.tell()?;

            let sections_iter = zoom.2.map(|mut section| {
                section.offset += zoom_data_offset;
                section
            });

            zoom_file.seek(SeekFrom::Start(0))?;
            let mut buf_reader = std::io::BufReader::new(zoom_file);
            std::io::copy(&mut buf_reader, &mut file)?;
            let zoom_index_offset = file.tell()?;
            //println!("Zoom {:?}, data: {:?}, offset {:?}", zoom.0, zoom_data_offset, zoom_index_offset);
            assert_eq!(zoom_index_offset - zoom_data_offset, zoom_size);
            let (nodes, levels, total_sections) = BigWigWrite::get_rtreeindex(sections_iter, options);
            BigWigWrite::write_rtreeindex(&mut file, nodes, levels, total_sections, options)?;

            zoom_entries.push(ZoomHeader {
                reduction_level: zoom.0,
                data_offset: zoom_data_offset,
                index_offset: zoom_index_offset,
            });

            zoom_count += 1;
            if zoom_count >= BigWigWrite::MAX_ZOOM_LEVELS {
                break;
            }
        }

        Ok(())
    }

    fn get_rtreeindex<S>(sections_stream: S, options: &BigWigWriteOptions) -> (RTreeNodeList<RTreeNode>, usize, u64) where S : Iterator<Item=Section> {
        let mut total_sections = 0;
        let mut current_nodes: Box<Iterator<Item=RTreeNode>> = Box::new(sections_stream.map(|s| RTreeNode {
            start_chrom_idx: s.chrom,
            start_base: s.start,
            end_chrom_idx: s.chrom,
            end_base: s.end,
            kind: RTreeNodeType::Leaf {
                offset: s.offset,
                size: s.size,
            },
        }));
        let mut levels = 0;
        let nodes: RTreeNodeList<RTreeNode> = loop {
            let mut start_chrom_idx = 0;
            let mut start_base = 0;
            let mut end_chrom_idx = 0;
            let mut end_base = 0;
            let mut next_nodes: Vec<RTreeNode> = vec![];
            let mut current_group: Vec<RTreeNode> = vec![];
            let mut levelup = false;
            loop {
                let next_node = current_nodes.next();
                match next_node {
                    None => {
                        //println!("Remaining nodes at complete: {}", current_group.len());
                        if current_group.len() > 0 {
                            if next_nodes.is_empty() {
                                next_nodes = current_group;
                            } else {
                                next_nodes.push(RTreeNode{
                                    start_chrom_idx,
                                    start_base,
                                    end_chrom_idx,
                                    end_base,
                                    kind: RTreeNodeType::NonLeaf {
                                        children: RTreeNodeList::<RTreeNode> {
                                            nodes: current_group
                                        }
                                    },
                                });
                            }
                        }
                        break
                    },
                    Some(node) => {
                        if levels == 0 {
                            total_sections += 1;
                        }
                        if current_group.is_empty() {
                            start_chrom_idx = node.start_chrom_idx;
                            start_base = node.start_base;
                            end_chrom_idx = node.end_chrom_idx;
                            end_base = node.end_base;
                        } else {
                            if end_chrom_idx == node.end_chrom_idx {
                                end_base = std::cmp::max(end_base, node.end_base);
                            } else {
                                end_base = node.end_base
                            }
                            end_chrom_idx = std::cmp::max(end_chrom_idx, node.end_chrom_idx);
                        }
                        current_group.push(node);
                        if current_group.len() >= options.block_size as usize {
                            if !levelup {
                                levels += 1;
                                levelup = true;
                            }
                            next_nodes.push(RTreeNode{
                                start_chrom_idx,
                                start_base,
                                end_chrom_idx,
                                end_base,
                                kind: RTreeNodeType::NonLeaf {
                                    children: RTreeNodeList::<RTreeNode> {
                                        nodes: current_group
                                    }
                                },
                            });
                            current_group = vec![];
                        }
                    }
                }
            }

            if next_nodes.len() < options.block_size as usize {
                break RTreeNodeList::<RTreeNode> {
                    nodes: next_nodes
                }
            }

            current_nodes = Box::new(next_nodes.into_iter());
        };
        //println!("Total sections: {:?}", total_sections);
        //println!("Nodes ({:?}): {:?}", nodes.nodes.len(), nodes);
        //println!("Levels: {:?}", levels);
        (nodes, levels, total_sections)
    }

    fn write_rtreeindex(file: &mut BufWriter<File>, nodes: RTreeNodeList<RTreeNode>, levels: usize, section_count: u64, options: &BigWigWriteOptions) -> std::io::Result<()> {
        const NODEHEADER_SIZE: u64 = 1 + 1 + 2;
        const NON_LEAFNODE_SIZE: u64 = 4 + 4 + 4 + 4 + 8;
        const LEAFNODE_SIZE: u64 = 4 + 4 + 4 + 4 + 8 + 8;

        let mut index_offsets: Vec<u64> = vec![0u64; levels as usize];

        fn calculate_offsets(mut index_offsets: &mut Vec<u64>, trees: &RTreeNodeList<RTreeNode>, level: usize) -> std::io::Result<()> {
            if level == 0 {
                return Ok(())
            }
            let isleaf: bool = {
                if trees.nodes.is_empty() {
                    false
                } else {
                    match trees.nodes[0].kind {
                        RTreeNodeType::Leaf { .. } => true,
                        RTreeNodeType::NonLeaf { .. } => false,
                    }
                }
            };
            index_offsets[level - 1] += NODEHEADER_SIZE;
            for tree in trees.nodes.iter() {
                match &tree.kind {
                    RTreeNodeType::Leaf { .. } => panic!("Only calculating offsets/sizes for indices (level > 0)"),
                    RTreeNodeType::NonLeaf { children, .. } => {
                        debug_assert!(level != 0, "Non Leaf node found at level 0");
                        debug_assert!(!isleaf, "Mixed node types at level {}", level);

                        index_offsets[level - 1] += NON_LEAFNODE_SIZE;

                        calculate_offsets(&mut index_offsets, &children, level - 1)?;
                    },
                }
            }
            Ok(())
        }

        calculate_offsets(&mut index_offsets, &nodes, levels)?;
        //println!("index Offsets: {:?}", index_offsets);

        fn write_tree(mut file: &mut BufWriter<File>, trees: &RTreeNodeList<RTreeNode>, curr_level: usize, dest_level: usize, childnode_offset: u64, options: &BigWigWriteOptions) -> std::io::Result<u64> {
            let NON_LEAFNODE_FULL_BLOCK_SIZE: u64 = NODEHEADER_SIZE + NON_LEAFNODE_SIZE * options.block_size as u64;
            let LEAFNODE_FULL_BLOCK_SIZE: u64 = NODEHEADER_SIZE + LEAFNODE_SIZE * options.block_size as u64;
            assert!(curr_level >= dest_level);
            let mut total_size = 0;
            if curr_level != dest_level {
                let mut next_offset_offset = 0;
                for tree in trees.nodes.iter() {
                    match &tree.kind {
                        RTreeNodeType::Leaf { .. } => panic!("Leaf node found at level {}", curr_level),
                        RTreeNodeType::NonLeaf { children, .. } => {
                            debug_assert!(curr_level != 0);
                            next_offset_offset += write_tree(&mut file, &children, curr_level - 1, dest_level, childnode_offset + next_offset_offset, options)?;
                        },
                    }
                }
                total_size += next_offset_offset;
                return Ok(total_size)
            }
            let isleaf = if trees.nodes.len() == 0 {
                0
            } else if let RTreeNodeType::Leaf { .. } = trees.nodes[0].kind {
                1
            } else {
                0
            };

            //println!("Writing {}. Isleaf: {} At: {}", trees.nodes.len(), isleaf, file.seek(SeekFrom::Current(0))?);
            //println!("Level: {:?}", curr_level);
            file.write_u8(isleaf)?;
            file.write_u8(0)?;
            file.write_u16::<NativeEndian>(trees.nodes.len() as u16)?;
            total_size += 4;
            for (idx, node) in trees.nodes.iter().enumerate() {
                file.write_u32::<NativeEndian>(node.start_chrom_idx)?;
                file.write_u32::<NativeEndian>(node.start_base)?;
                file.write_u32::<NativeEndian>(node.end_chrom_idx)?;
                file.write_u32::<NativeEndian>(node.end_base)?;
                total_size += 16;
                match &node.kind {
                    RTreeNodeType::Leaf { offset, size } => {
                        file.write_u64::<NativeEndian>(*offset)?;
                        file.write_u64::<NativeEndian>(*size)?;
                        total_size += 16;
                    },
                    RTreeNodeType::NonLeaf { .. } => {
                        debug_assert!(curr_level != 0);
                        let full_size = if (curr_level - 1) > 0 {
                            NON_LEAFNODE_FULL_BLOCK_SIZE
                        } else {
                            LEAFNODE_FULL_BLOCK_SIZE
                        };
                        let child_offset: u64 = childnode_offset + idx as u64 * full_size;
                        //println!("Child node offset: {}; Added: {}", child_offset, idx as u64 * full_size);
                        file.write_u64::<NativeEndian>(child_offset)?;
                        total_size += 8;
                    },
                }
            }
            Ok(total_size)
        }


        let end_of_data = file.seek(SeekFrom::Current(0))?;
        {
            //println!("cirTree header (write):\n bs: {:?}\n ic: {:?}\n sci: {:?}\n sb: {:?}\n eci: {:?}\n eb: {:?}\n efo: {:?}\n ips: {:?}\n r: {:?}", BLOCK_SIZE, section_count, nodes.nodes[0].start_chrom_idx, nodes.nodes[0].start_base, nodes.nodes[nodes.nodes.len() - 1].end_chrom_idx, nodes.nodes[nodes.nodes.len() - 1].end_base, end_of_data, ITEMS_PER_SLOT, 0);
            file.write_u32::<NativeEndian>(CIR_TREE_MAGIC)?;
            file.write_u32::<NativeEndian>(options.block_size)?;
            file.write_u64::<NativeEndian>(section_count)?;
            if nodes.nodes.len() == 0 {
                file.write_u32::<NativeEndian>(0)?;
                file.write_u32::<NativeEndian>(0)?;
                file.write_u32::<NativeEndian>(0)?;
                file.write_u32::<NativeEndian>(0)?;
            } else {
                file.write_u32::<NativeEndian>(nodes.nodes[0].start_chrom_idx)?;
                file.write_u32::<NativeEndian>(nodes.nodes[0].start_base)?;
                file.write_u32::<NativeEndian>(nodes.nodes[nodes.nodes.len() - 1].end_chrom_idx)?;
                file.write_u32::<NativeEndian>(nodes.nodes[nodes.nodes.len() - 1].end_base)?;
            }
            file.write_u64::<NativeEndian>(end_of_data)?;
            file.write_u32::<NativeEndian>(options.items_per_slot)?;
            file.write_u32::<NativeEndian>(0)?;
        }

        let mut next_offset = file.seek(SeekFrom::Current(0))?;
        //println!("Levels: {:?}", levels);
        //println!("Start of index: {}", next_offset);
        for level in (0..=levels).rev() {
            if level > 0 {
                next_offset += index_offsets[level - 1];
            }
            write_tree(file, &nodes, levels, level, next_offset, options)?;
            //println!("End of index level {}: {}", level, file.seek(SeekFrom::Current(0))?);
        }

        Ok(())
    }
}

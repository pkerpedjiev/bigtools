//! There are roughly three layers of abstraction here, each with a different purpose.
//!
//! The first layer of abstraction is enscapsulated in the `StreamingBedValues` trait. Briefly,
//! implementors of this trait return "raw" bed-like data. This is the chromosome (as a `&str`) and
//! data specific to each type of bed
//!
//! The second layer of abstraction manages the state information for when values switch from one
//! chromosome to another. The is important because bigwig/bigbed writing is "chunked" by chromosome.
//!
//! The final layer of abstraction is a thin wrapper around the previous to provide some optional
//! error checking and to keep track of the chromosomes seen.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::hash::BuildHasher;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_utils::atomic::AtomicCell;
use futures::executor::ThreadPool;
use thiserror::Error;

use crate::bigwig::{BedEntry, Value};
use crate::utils::chromvalues::ChromValues;
use crate::utils::idmap::IdMap;
use crate::utils::streaming_linereader::StreamingLineReader;
use crate::{
    BBIWriteOptions, ChromData, ChromDataState, ChromProcessingFnOutput, ChromProcessingOutput,
    ReadData, WriteGroupsError, WriteSummaryFuture,
};

// FIXME: replace with LendingIterator when GATs are thing
/// Essentially a combined lending iterator over the chrom (&str) and remaining
/// values of bed-like data
pub trait StreamingBedValues {
    type Value;

    fn next(&mut self) -> Option<io::Result<(&str, Self::Value)>>;
}

// ---------------
// Bed-like stream
// ---------------

pub type Parser<V> = for<'a> fn(&'a str) -> Option<io::Result<(&'a str, V)>>;

/// Parses a bed-like file
pub struct BedFileStream<V, B> {
    bed: StreamingLineReader<B>,
    parse: Parser<V>,
}

impl<V, B: BufRead> StreamingBedValues for BedFileStream<V, B> {
    type Value = V;

    fn next(&mut self) -> Option<io::Result<(&str, Self::Value)>> {
        let line = match self.bed.read()? {
            Ok(line) => line.trim_end(),
            Err(e) => return Some(Err(e)),
        };
        (self.parse)(line)
    }
}

// Wraps a bed-like Iterator
pub struct BedIteratorStream<V, I> {
    iter: I,
    curr: Option<(String, V)>,
}

impl<V: Clone, I: Iterator<Item = io::Result<(String, V)>>> StreamingBedValues
    for BedIteratorStream<V, I>
{
    type Value = V;

    fn next(&mut self) -> Option<io::Result<(&str, V)>> {
        use std::ops::Deref;
        self.curr = match self.iter.next()? {
            Err(e) => return Some(Err(e)),
            Ok(v) => Some(v),
        };
        self.curr.as_ref().map(|v| Ok((v.0.deref(), v.1.clone())))
    }
}

// ----------------
// State-management
// ----------------

/// A wrapper for "bed-like" data
pub struct BedParser<S: StreamingBedValues> {
    state: Arc<AtomicCell<Option<BedParserState<S>>>>,
}

#[derive(Debug)]
enum ChromOpt {
    None,
    Same,
    Diff(String),
}

// Example order of state transitions
// 1) active_chrom: None, next_val: None (creation)
// 2) active_chrom: Some(X), next_val: Some((.., Same)) (load value)
// 3) active_chrom: Some(X), next_val: None (value taken)
// (cycle between for 2 and 3 for all values of a chromosome)
// 4) active_chrom: None, next_val: Some((.., Diff(Y))) (switch chromosome)
// 5) active_chrom: Some(Y), next_val: Some((.. Same)) (load value)
// 6) active_chrom: Some(Y), next_val: None (value taken)
// (cycle between 5 and 6 for all values of a chromosome)
#[derive(Debug)]
struct BedParserState<S: StreamingBedValues> {
    stream: S,
    active_chrom: Option<String>,
    next_val: Option<(S::Value, ChromOpt)>,
}

impl<S: StreamingBedValues> BedParser<S> {
    pub fn new(stream: S) -> Self {
        let state = BedParserState {
            stream,
            active_chrom: None,
            next_val: None,
        };
        BedParser {
            state: Arc::new(AtomicCell::new(Some(state))),
        }
    }
}

pub fn parse_bed<'a>(s: &'a str) -> Option<io::Result<(&'a str, BedEntry)>> {
    let mut split = s.splitn(4, '\t');
    let chrom = match split.next() {
        Some(chrom) => chrom,
        None => return None,
    };
    let res = (|| {
        let s = split.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Missing start: {:}", s))
        })?;
        let start = s.parse::<u32>().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Invalid start: {:}", s))
        })?;
        let s = split.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Missing end: {:}", s))
        })?;
        let end = s.parse::<u32>().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Invalid end: {:}", s))
        })?;
        let rest = split.next().unwrap_or("").to_string();
        Ok((start, end, rest))
    })();
    match res {
        Err(e) => Some(Err(e)),
        Ok((start, end, rest)) => Some(Ok((chrom, BedEntry { start, end, rest }))),
    }
}

pub fn parse_bedgraph<'a>(s: &'a str) -> Option<io::Result<(&'a str, Value)>> {
    let mut split = s.splitn(5, '\t');
    let chrom = match split.next() {
        Some(chrom) => chrom,
        None => return None,
    };
    let res = (|| {
        let s = split.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Missing start: {:}", s))
        })?;
        let start = s.parse::<u32>().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Invalid start: {:}", s))
        })?;
        let s = split.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Missing end: {:}", s))
        })?;
        let end = s.parse::<u32>().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Invalid end: {:}", s))
        })?;
        let s = split.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Missing value: {:}", s))
        })?;
        let value = s.parse::<f32>().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Invalid value: {:}", s))
        })?;
        Ok((start, end, value))
    })();
    match res {
        Err(e) => Some(Err(e)),
        Ok((start, end, value)) => Some(Ok((chrom, Value { start, end, value }))),
    }
}

impl BedParser<BedFileStream<BedEntry, BufReader<File>>> {
    pub fn from_bed_file(file: File) -> Self {
        BedParser::new(BedFileStream {
            bed: StreamingLineReader::new(BufReader::new(file)),
            parse: parse_bed,
        })
    }
}

impl<R: Read> BedParser<BedFileStream<Value, BufReader<R>>> {
    pub fn from_bedgraph_file(file: R) -> Self {
        BedParser::new(BedFileStream {
            bed: StreamingLineReader::new(BufReader::new(file)),
            parse: parse_bedgraph,
        })
    }
}

impl<V: Clone, I: Iterator<Item = io::Result<(String, V)>>> BedParser<BedIteratorStream<V, I>> {
    pub fn wrap_iter(iter: I) -> Self {
        BedParser::new(BedIteratorStream { iter, curr: None })
    }
}

impl<S: StreamingBedValues> BedParser<S> {
    // This is *valid* to call multiple times for the same chromosome (assuming the
    // `BedChromData` has been dropped), since calling this function doesn't
    // actually advance the state (it will only set `next_val` if it currently is none).
    pub fn next_chrom(&mut self) -> Result<Option<(String, BedChromData<S>)>, BedParseError> {
        let mut state = self.state.swap(None).expect("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
        state.load_state(true)?;
        let chrom = state.active_chrom.clone();
        self.state.swap(Some(state));

        match chrom {
            Some(chrom) => {
                let group = BedChromData {
                    state: self.state.clone(),
                    curr_state: None,
                    done: false,
                };
                Ok(Some((chrom.to_owned(), group)))
            }
            None => Ok(None),
        }
    }
}

impl<S: StreamingBedValues> BedParserState<S> {
    fn load_state(&mut self, switch_chrom: bool) -> Result<(), BedParseError> {
        if !switch_chrom && self.active_chrom.is_none() {
            return Ok(());
        }
        match (&self.active_chrom, self.next_val.take()) {
            (None, Some((_, ChromOpt::Same))) => panic!(),
            (None, Some((v, ChromOpt::Diff(chrom)))) => {
                self.active_chrom = Some(chrom);
                self.next_val = Some((v, ChromOpt::Same));
                return Ok(());
            }
            (Some(_), Some(next_val)) => {
                self.next_val = Some(next_val);
                return Ok(());
            }
            _ => {}
        }

        if let Some(next) = self.stream.next() {
            let (chrom, v) = next?;
            let next_chrom = match &self.active_chrom {
                // If the chromosome read is the same as the active chromosome,
                // then nothing to do other than return `Same`
                Some(curr_chrom) if curr_chrom == chrom => ChromOpt::Same,
                // If it's the first, set as active and return `Same`
                None => {
                    self.active_chrom = Some(chrom.to_owned());
                    ChromOpt::Same
                }
                // Otherwise, it's different, so set active to none and return Diff
                Some(_) => {
                    self.active_chrom = None;
                    ChromOpt::Diff(chrom.to_owned())
                }
            };
            self.next_val = Some((v, next_chrom));
        } else {
            self.active_chrom = None;
        }

        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum BedParseError {
    #[error("{}", .0)]
    InvalidInput(String),
    #[error("{}", .0)]
    IoError(io::Error),
}

impl From<io::Error> for BedParseError {
    fn from(e: io::Error) -> Self {
        Self::IoError(e)
    }
}

// The separation here between the "current" state and the shared state comes
// from the observation that once we *start* on a chromosome, we can't move on
// to the next until we've exhausted the current. In this *particular*
// implementation, we don't allow parallel iteration of chromsomes. So, the
// state is either needed *here* or in the main struct.
pub struct BedChromData<S: StreamingBedValues> {
    state: Arc<AtomicCell<Option<BedParserState<S>>>>,
    curr_state: Option<BedParserState<S>>,
    done: bool,
}

impl<S: StreamingBedValues> ChromValues for BedChromData<S> {
    type Value = S::Value;
    type Error = BedParseError;

    fn next(&mut self) -> Option<Result<Self::Value, Self::Error>> {
        if self.curr_state.is_none() {
            let opt_state = self.state.swap(None);
            if opt_state.is_none() {
                panic!("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
            }
            self.curr_state = opt_state;
        }
        if self.done {
            return None;
        }
        let state = self.curr_state.as_mut().unwrap();
        if let Err(e) = state.load_state(false) {
            return Some(Err(e));
        }
        if state.active_chrom.is_none() {
            self.done = true;
            return None;
        }

        let next_val = state.next_val.take()?;
        Some(Ok(next_val.0))
    }

    fn peek(&mut self) -> Option<Result<&S::Value, Self::Error>> {
        if self.curr_state.is_none() {
            let opt_state = self.state.swap(None);
            if opt_state.is_none() {
                panic!("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
            }
            self.curr_state = opt_state;
        }
        if self.done {
            return None;
        }
        let state = self.curr_state.as_mut().unwrap();
        if let Err(e) = state.load_state(false) {
            return Some(Err(e));
        }
        if state.active_chrom.is_none() {
            self.done = true;
            return None;
        }
        let state = self.curr_state.as_ref().unwrap();
        state.next_val.as_ref().map(|v| Ok(&v.0))
    }
}

impl<S: StreamingBedValues> Drop for BedChromData<S> {
    fn drop(&mut self) {
        if let Some(state) = self.curr_state.take() {
            self.state.swap(Some(state));
        }
    }
}

// ------------------------------------------------
// Chromosome tracking and optional error reporting
// ------------------------------------------------

pub struct BedParserStreamingIterator<S: StreamingBedValues, H: BuildHasher> {
    bed_data: BedParser<S>,
    chrom_map: HashMap<String, u32, H>,
    allow_out_of_order_chroms: bool,
    chrom_ids: Option<IdMap>,
    last_chrom: Option<String>,
}

impl<S: StreamingBedValues, H: BuildHasher> BedParserStreamingIterator<S, H> {
    pub fn new(
        bed_data: BedParser<S>,
        chrom_map: HashMap<String, u32, H>,
        allow_out_of_order_chroms: bool,
    ) -> Self {
        BedParserStreamingIterator {
            bed_data,
            chrom_map,
            allow_out_of_order_chroms,
            chrom_ids: Some(IdMap::default()),
            last_chrom: None,
        }
    }
}

impl<S: StreamingBedValues, H: BuildHasher> ChromData for BedParserStreamingIterator<S, H> {
    type Output = BedChromData<S>;

    /// Advancing after `ChromDataState::Finished` has been called will result in a panic.
    fn advance<
        F: Fn(ReadData<Self::Output>) -> io::Result<ChromProcessingFnOutput<Self::Output>>,
    >(
        &mut self,
        do_read: &F,
    ) -> io::Result<ChromDataState<Self::Output>> {
        Ok(match self.bed_data.next_chrom() {
            Err(err) => ChromDataState::Error(err.into()),
            Ok(Some((chrom, group))) => {
                let chrom_ids = self.chrom_ids.as_mut().unwrap();

                // First, if we don't want to allow out of order chroms, error here
                let last = self.last_chrom.replace(chrom.clone());
                if let Some(c) = last {
                    // TODO: test this correctly fails
                    if !self.allow_out_of_order_chroms && c >= chrom {
                        return Ok(ChromDataState::Error(BedParseError::InvalidInput("Input bedGraph not sorted by chromosome. Sort with `sort -k1,1 -k2,2n`.".to_string())));
                    }
                }

                // Next, make sure we have the length of the chromosome
                let length = match self.chrom_map.get(&chrom) {
                    Some(length) => *length,
                    None => return Ok(ChromDataState::Error(BedParseError::InvalidInput(format!("Input bedGraph contains chromosome that isn't in the input chrom sizes: {}", chrom)))),
                };
                // Make a new id for the chromosome
                let chrom_id = chrom_ids.get_id(&chrom);

                let read_data = (chrom, chrom_id, length, group);
                let read = do_read(read_data)?;
                ChromDataState::NewChrom(read)
            }
            Ok(None) => {
                let chrom_ids = self.chrom_ids.take().unwrap();
                ChromDataState::Finished(chrom_ids)
            }
        })
    }
}

/*
pub struct BedParserParallelStreamingIterator<F, V, H: BuildHasher>
where F: Fn(
    ReadData<V>,
    ThreadPool,
    BBIWriteOptions,
) -> io::Result<(WriteSummaryFuture, ChromProcessingOutput)>
{
    begin_processing_chrom: F,
    parse_fn: Parser<V>,
    chrom_map: HashMap<String, u32, H>,
    allow_out_of_order_chroms: bool,
    pool: ThreadPool,
    path: PathBuf,
    chrom_ids: Option<IdMap>,
    last_chrom: Option<String>,
    chrom_indices: Vec<(u64, String)>,
    max_idxs: usize,
    queued_reads: VecDeque<()>,
}

impl<F, V, H: BuildHasher> BedParserParallelStreamingIterator<F, V, H>
where F: Fn(
    ReadData<V>,
    ThreadPool,
    BBIWriteOptions,
) -> io::Result<(WriteSummaryFuture, ChromProcessingOutput)>
{
    pub fn new(
        begin_processing_chrom: F,
        parse_fn: Parser<V>,
        chrom_map: HashMap<String, u32, H>,
        mut chrom_indices: Vec<(u64, String)>,
        allow_out_of_order_chroms: bool,
        pool: ThreadPool,
        path: PathBuf,
    ) -> Self {
        // For speed, we `pop` and go in reverse order. We want forward order,
        // so reverse here.
        chrom_indices.reverse();

        BedParserParallelStreamingIterator {
            begin_processing_chrom,
            parse_fn,
            chrom_map,
            allow_out_of_order_chroms,
            pool,
            path,
            chrom_ids: Some(IdMap::default()),
            last_chrom: None,
            chrom_indices,
            max_idxs: 2,
            queued_reads: VecDeque::new(),
        }
    }
}

impl<F, S: StreamingBedValues, H: BuildHasher> ChromData for BedParserParallelStreamingIterator<F, S, H> where F: Fn(
    ReadData<S>,
    ThreadPool,
    BBIWriteOptions,
) -> io::Result<(WriteSummaryFuture, ChromProcessingOutput)> {
    type Output = BedChromData<BedFileStream<S, BufReader<File>>>;

    fn advance(mut self) -> ChromDataState<Self> {
        let begin_next = |_self: Self| {
            let curr = match _self.chrom_indices.pop() {
                Some(c) => c,
                None => {
                    let chrom_ids = _self.chrom_ids.take().unwrap();
                    return ChromDataState::Finished(chrom_ids);
                },
            };
            let chrom = curr.1;

            let file = match File::open(&_self.path) {
                Ok(f) => f,
                Err(err) => return ChromDataState::Error(err.into()),
            };
            file.seek(SeekFrom::Start(curr.0));
            let parser = BedParser::new(BedFileStream {
                bed: StreamingLineReader::new(BufReader::new(file)),
                parse: _self.parse_fn,
            });

            match parser.next_chrom() {
                Some(Err(err)) => ChromDataState::Error(err.into()),
                Some(Ok((chrom, group))) => {
                    let chrom_ids = _self.chrom_ids.as_mut().unwrap();
                    let last = _self.last_chrom.replace(chrom.clone());
                    if let Some(c) = last {
                        // TODO: test this correctly fails
                        if !self.allow_out_of_order_chroms && c >= chrom {
                            return ChromDataState::Error(WriteGroupsError::InvalidInput("Input bedGraph not sorted by chromosome. Sort with `sort -k1,1 -k2,2n`.".to_string()));
                        }
                    }
                    let length = match _self.chrom_map.get(&chrom) {
                        Some(length) => *length,
                        None => return ChromDataState::Error(WriteGroupsError::InvalidInput(format!("Input bedGraph contains chromosome that isn't in the input chrom sizes: {}", chrom))),
                    };
                    let chrom_id = chrom_ids.get_id(&chrom);
                    let read_data = (chrom, chrom_id, length, group);

                    ChromDataState::Read(read_data, _self)
                }
                None => {
                    let chrom_ids = _self.chrom_ids.take().unwrap();
                    ChromDataState::Finished(chrom_ids)
                }
            }
        };

        let next = self
            .queued_reads
            .pop_front()
            .unwrap_or_else(|| {
                /*
                let next_chrom = begin_next(self);
                match next_chrom {
                    ChromDataState::Read(read_data, _self) => {},
                    ChromDataState::Finished(chrom_ids) => todo!(),
                    ChromDataState::Error(error) => todo!(),
                }
                Some(())
                */
                begin_next(self)
            });
    }
}
 */

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io;
    use std::path::PathBuf;

    #[test]
    fn test_bed_works() -> io::Result<()> {
        let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        dir.push("resources/test");
        dir.push("small.bed");
        let f = File::open(dir)?;
        let mut bgp = BedParser::from_bed_file(f);
        {
            let (chrom, mut group) = bgp.next_chrom().unwrap().unwrap();
            assert_eq!(chrom, "chr17");
            assert_eq!(
                &BedEntry {
                    start: 1,
                    end: 100,
                    rest: "test1\t0".to_string()
                },
                group.peek().unwrap().unwrap()
            );
        }
        {
            let (chrom, mut group) = bgp.next_chrom().unwrap().unwrap();
            assert_eq!(chrom, "chr17");
            assert_eq!(
                &BedEntry {
                    start: 1,
                    end: 100,
                    rest: "test1\t0".to_string()
                },
                group.peek().unwrap().unwrap()
            );
        }
        {
            let (chrom, mut group) = bgp.next_chrom().unwrap().unwrap();
            assert_eq!(chrom, "chr17");
            assert_eq!(
                &BedEntry {
                    start: 1,
                    end: 100,
                    rest: "test1\t0".to_string()
                },
                group.peek().unwrap().unwrap()
            );
            assert_eq!(chrom, "chr17");
            assert_eq!(
                BedEntry {
                    start: 1,
                    end: 100,
                    rest: "test1\t0".to_string()
                },
                group.next().unwrap().unwrap()
            );
            assert_eq!(
                &BedEntry {
                    start: 101,
                    end: 200,
                    rest: "test2\t0".to_string()
                },
                group.peek().unwrap().unwrap()
            );
            assert_eq!(
                &BedEntry {
                    start: 101,
                    end: 200,
                    rest: "test2\t0".to_string()
                },
                group.peek().unwrap().unwrap()
            );

            assert_eq!(
                BedEntry {
                    start: 101,
                    end: 200,
                    rest: "test2\t0".to_string()
                },
                group.next().unwrap().unwrap()
            );
            assert_eq!(
                &BedEntry {
                    start: 201,
                    end: 300,
                    rest: "test3\t0".to_string()
                },
                group.peek().unwrap().unwrap()
            );

            assert_eq!(
                BedEntry {
                    start: 201,
                    end: 300,
                    rest: "test3\t0".to_string()
                },
                group.next().unwrap().unwrap()
            );
            assert!(group.peek().is_none());

            assert!(group.next().is_none());
            assert!(group.peek().is_none());
        }
        {
            let (chrom, mut group) = bgp.next_chrom().unwrap().unwrap();
            assert_eq!(chrom, "chr18");
            assert_eq!(
                BedEntry {
                    start: 1,
                    end: 100,
                    rest: "test4\t0".to_string()
                },
                group.next().unwrap().unwrap()
            );
            assert_eq!(
                &BedEntry {
                    start: 101,
                    end: 200,
                    rest: "test5\t0".to_string()
                },
                group.peek().unwrap().unwrap()
            );
            assert_eq!(
                &BedEntry {
                    start: 101,
                    end: 200,
                    rest: "test5\t0".to_string()
                },
                group.peek().unwrap().unwrap()
            );

            assert_eq!(
                BedEntry {
                    start: 101,
                    end: 200,
                    rest: "test5\t0".to_string()
                },
                group.next().unwrap().unwrap()
            );
            assert!(group.peek().is_none());

            assert!(group.next().is_none());
            assert!(group.peek().is_none());
        }
        {
            let (chrom, mut group) = bgp.next_chrom().unwrap().unwrap();
            assert_eq!(chrom, "chr19");
            assert_eq!(
                BedEntry {
                    start: 1,
                    end: 100,
                    rest: "test6\t0".to_string()
                },
                group.next().unwrap().unwrap()
            );
            assert!(group.peek().is_none());

            assert!(group.next().is_none());
            assert!(group.peek().is_none());
        }
        assert!(matches!(bgp.next_chrom(), Ok(None)));
        Ok(())
    }

    #[test]
    fn test_bedgraph_works() -> io::Result<()> {
        let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        dir.push("resources/test");
        dir.push("small.bedGraph");
        let f = File::open(dir)?;
        let mut bgp = BedParser::from_bedgraph_file(f);
        {
            let (chrom, mut group) = bgp.next_chrom().unwrap().unwrap();
            assert_eq!(chrom, "chr17");
            assert_eq!(
                Value {
                    start: 1,
                    end: 100,
                    value: 0.5
                },
                group.next().unwrap().unwrap()
            );
            assert_eq!(
                &Value {
                    start: 101,
                    end: 200,
                    value: 0.5
                },
                group.peek().unwrap().unwrap()
            );
            assert_eq!(
                &Value {
                    start: 101,
                    end: 200,
                    value: 0.5
                },
                group.peek().unwrap().unwrap()
            );

            assert_eq!(
                Value {
                    start: 101,
                    end: 200,
                    value: 0.5
                },
                group.next().unwrap().unwrap()
            );
            assert_eq!(
                &Value {
                    start: 201,
                    end: 300,
                    value: 0.5
                },
                group.peek().unwrap().unwrap()
            );

            assert_eq!(
                Value {
                    start: 201,
                    end: 300,
                    value: 0.5
                },
                group.next().unwrap().unwrap()
            );
            assert!(group.peek().is_none());

            assert!(group.next().is_none());
            assert!(group.peek().is_none());
        }
        {
            let (chrom, mut group) = bgp.next_chrom().unwrap().unwrap();
            assert_eq!(chrom, "chr18");
            assert_eq!(
                Value {
                    start: 1,
                    end: 100,
                    value: 0.5
                },
                group.next().unwrap().unwrap()
            );
            assert_eq!(
                &Value {
                    start: 101,
                    end: 200,
                    value: 0.5
                },
                group.peek().unwrap().unwrap()
            );
            assert_eq!(
                &Value {
                    start: 101,
                    end: 200,
                    value: 0.5
                },
                group.peek().unwrap().unwrap()
            );

            assert_eq!(
                Value {
                    start: 101,
                    end: 200,
                    value: 0.5
                },
                group.next().unwrap().unwrap()
            );
            assert!(group.peek().is_none());

            assert!(group.next().is_none());
            assert!(group.peek().is_none());
        }
        {
            let (chrom, mut group) = bgp.next_chrom().unwrap().unwrap();
            assert_eq!(chrom, "chr19");
            assert_eq!(
                Value {
                    start: 1,
                    end: 100,
                    value: 0.5
                },
                group.next().unwrap().unwrap()
            );
            assert!(group.peek().is_none());

            assert!(group.next().is_none());
            assert!(group.peek().is_none());
        }
        assert!(matches!(bgp.next_chrom(), Ok(None)));
        Ok(())
    }
}

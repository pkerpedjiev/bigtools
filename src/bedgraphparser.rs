use std::collections::HashMap;
use std::fs::File;
use std::hash::BuildHasher;
use std::io::{self, BufRead, BufReader};
use std::sync::Arc;

use futures::future::Either;

use crate::bigwig::BBIWriteOptions;
use crate::bigwig::BigWigWrite;
use crate::bigwig::ChromGroupRead;
use crate::bigwig::ChromGroupReadStreamingIterator;
use crate::bigwig::Value;
use crate::bigwig::WriteGroupsError;
use crate::idmap::IdMap;
use crate::streaming_linereader::StreamingLineReader;
use crate::chromvalues::{ChromGroups, ChromValues};

use crossbeam::atomic::AtomicCell;


pub trait StreamingChromValues {
    fn next<'a>(&'a mut self) -> io::Result<Option<(&'a str, u32, u32, f32)>>;
}

pub struct BedGraphStream<B: BufRead> {
    bedgraph: StreamingLineReader<B>
}

impl<B: BufRead> StreamingChromValues for BedGraphStream<B> {
    fn next<'a>(&'a mut self) -> io::Result<Option<(&'a str, u32, u32, f32)>> {
        let l = self.bedgraph.read()?;
        let line = match l {
            Some(line) => line,
            None => return Ok(None),
        };
        let mut split = line.split_whitespace();
        let chrom = match split.next() {
            Some(chrom) => chrom,
            None => {
                return Ok(None);
            },
        };
        let start = split.next().expect("Missing start").parse::<u32>().unwrap();
        let end = split.next().expect("Missing end").parse::<u32>().unwrap();
        let value = split.next().expect("Missing value").parse::<f32>().unwrap();
        Ok(Some((chrom, start, end, value)))
    }
}

pub struct BedGraphIteratorStream<I: Iterator<Item=io::Result<(String, u32, u32, f32)>>> {
    iter: I,
    curr: Option<(String, u32, u32, f32)>,
}

impl<I: Iterator<Item=io::Result<(String, u32, u32, f32)>>> StreamingChromValues for BedGraphIteratorStream<I> {
    fn next<'a>(&'a mut self) -> io::Result<Option<(&'a str, u32, u32, f32)>> {
        use std::ops::Deref;
        self.curr = match self.iter.next() {
            None => return Ok(None),
            Some(v) => Some(v?),
        };
        Ok(self.curr.as_ref().map(|v| (v.0.deref(), v.1, v.2, v.3)))
    }
}

pub struct BedGraphParser<S: StreamingChromValues>{
    state: Arc<AtomicCell<Option<BedGraphParserState<S>>>>,
}

impl<S: StreamingChromValues> BedGraphParser<S> {
    pub fn new(stream: S) -> BedGraphParser<S> {
        let state = BedGraphParserState {
            stream,
            curr_chrom: None,
            next_chrom: ChromOpt::None,
            curr_val: None,
            next_val: None,
        };
        BedGraphParser {
            state: Arc::new(AtomicCell::new(Some(state))),
        }
    }
}

impl BedGraphParser<BedGraphStream<BufReader<File>>> {
    pub fn from_file(file: File) -> BedGraphParser<BedGraphStream<BufReader<File>>> {
        BedGraphParser::new(BedGraphStream { bedgraph: StreamingLineReader::new(BufReader::new(file)) })
    }
}

impl<I: Iterator<Item=io::Result<(String, u32, u32, f32)>>> BedGraphParser<BedGraphIteratorStream<I>> {
    pub fn from_iter(iter: I) -> BedGraphParser<BedGraphIteratorStream<I>> {
        BedGraphParser::new(BedGraphIteratorStream { iter, curr: None })
    }
}

#[derive(Debug)]
enum ChromOpt {
    None,
    Same,
    Diff(String),
}

#[derive(Debug)]
pub struct BedGraphParserState<S: StreamingChromValues> {
    stream: S,
    curr_chrom: Option<String>,
    curr_val: Option<Value>,
    next_chrom: ChromOpt,
    next_val: Option<Value>,
}

impl<S: StreamingChromValues> BedGraphParserState<S> {
    fn advance(&mut self) -> io::Result<()> {
        self.curr_val = self.next_val.take();
        match std::mem::replace(&mut self.next_chrom, ChromOpt::None) {
            ChromOpt::Diff(real_chrom) => {
                self.curr_chrom.replace(real_chrom);
            },
            ChromOpt::Same => {},
            ChromOpt::None => {
                self.curr_chrom = None;
            },
        }

        if let Some((chrom, start, end, value)) = self.stream.next()? {
            self.next_val.replace(Value { start, end, value });
            if let Some(curr_chrom) = &self.curr_chrom {
                if curr_chrom != chrom {
                    self.next_chrom = ChromOpt::Diff(chrom.to_owned());
                } else {
                    self.next_chrom = ChromOpt::Same;
                }
            } else {
                self.next_chrom = ChromOpt::Diff(chrom.to_owned());
            }
        }
        if self.curr_val.is_none() && self.next_val.is_some() {
            self.advance()?;
        }
        Ok(())
    }
}

impl<S: StreamingChromValues> ChromGroups<Value, ChromGroup<S>> for BedGraphParser<S> {
    fn next(&mut self) -> io::Result<Option<(String, ChromGroup<S>)>> {
        let mut state = self.state.swap(None).expect("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
        if let ChromOpt::Same = state.next_chrom {
            panic!("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
        }
        state.advance()?;

        let next_chrom = state.curr_chrom.as_ref();
        let ret = match next_chrom {
            None => Ok(None),
            Some(chrom) => {
                let group = ChromGroup { state: self.state.clone(), curr_state: None };
                Ok(Some((chrom.to_owned(), group)))
            },
        };
        self.state.swap(Some(state));
        ret
    }
}

pub struct ChromGroup<S: StreamingChromValues> {
    state: Arc<AtomicCell<Option<BedGraphParserState<S>>>>,
    curr_state: Option<BedGraphParserState<S>>,
}

impl<S: StreamingChromValues> ChromValues<Value> for ChromGroup<S> {
    fn next(&mut self) -> io::Result<Option<Value>> {
        if self.curr_state.is_none() {
            let opt_state = self.state.swap(None);
            if opt_state.is_none() {
                panic!("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
            }
            self.curr_state = opt_state;
        }
        let state = self.curr_state.as_mut().unwrap();
        if let Some(val) = state.curr_val.take() {
            return Ok(Some(val));
        }
        if let ChromOpt::Diff(_) = state.next_chrom {
            return Ok(None);
        }
        state.advance()?;
        Ok(state.curr_val.take())
    }

    fn peek(&mut self) -> Option<&Value> {
        if self.curr_state.is_none() {
            let opt_state = self.state.swap(None);
            if opt_state.is_none() {
                panic!("Invalid usage. This iterator does not buffer and all values should be exhausted for a chrom before next() is called.");
            }
            self.curr_state = opt_state;
        }
        let state = self.curr_state.as_ref().unwrap();
        if let ChromOpt::Diff(_) = state.next_chrom {
            return None;
        }
        state.next_val.as_ref()
    }
}

impl<S: StreamingChromValues> Drop for ChromGroup<S> {
    fn drop(&mut self) {
        if let Some(state) = self.curr_state.take() {
            self.state.swap(Some(state));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io;
    use std::path::PathBuf;
    extern crate test;

    #[test]
    fn test_works() -> io::Result<()> {
        let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        dir.push("resources/test");
        dir.push("small.bedGraph");
        let f = File::open(dir)?;
        let mut bgp = BedGraphParser::from_file(f);
        {
            let (chrom, mut group) = bgp.next()?.unwrap();
            assert_eq!(chrom, "chr17");
            assert_eq!(Value { start: 1, end: 100, value: 0.5 }, group.next()?.unwrap());
            assert_eq!(&Value { start: 101, end: 200, value: 0.5 }, group.peek().unwrap());
            assert_eq!(&Value { start: 101, end: 200, value: 0.5 }, group.peek().unwrap());

            assert_eq!(Value { start: 101, end: 200, value: 0.5 }, group.next()?.unwrap());
            assert_eq!(&Value { start: 201, end: 300, value: 0.5 }, group.peek().unwrap());

            assert_eq!(Value { start: 201, end: 300, value: 0.5 }, group.next()?.unwrap());
            assert_eq!(None, group.peek());

            assert_eq!(None, group.next()?);
            assert_eq!(None, group.peek());
        }
        {
            let (chrom, mut group) = bgp.next()?.unwrap();
            assert_eq!(chrom, "chr18");
            assert_eq!(Value { start: 1, end: 100, value: 0.5 }, group.next()?.unwrap());
            assert_eq!(&Value { start: 101, end: 200, value: 0.5 }, group.peek().unwrap());
            assert_eq!(&Value { start: 101, end: 200, value: 0.5 }, group.peek().unwrap());

            assert_eq!(Value { start: 101, end: 200, value: 0.5 }, group.next()?.unwrap());
            assert_eq!(None, group.peek());

            assert_eq!(None, group.next()?);
            assert_eq!(None, group.peek());
        }
        {
            let (chrom, mut group) = bgp.next()?.unwrap();
            assert_eq!(chrom, "chr19");
            assert_eq!(Value { start: 1, end: 100, value: 0.5 }, group.next()?.unwrap());
            assert_eq!(None, group.peek());

            assert_eq!(None, group.next()?);
            assert_eq!(None, group.peek());
        }
        assert!(bgp.next()?.is_none());
        Ok(())
    }

}

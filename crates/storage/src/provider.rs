use rand::RngCore;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write as _},
    os::unix::fs::FileExt,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use irys_types::CHUNK_SIZE;
use nodit::{
    interval::{ie, ii},
    InclusiveInterval, Interval, NoditMap,
};

use crate::{
    interval::{IntervalState, IntervalStateWrapped, PackingState},
    state::StorageModule,
};

#[derive(Debug)]
pub struct StorageProvider {
    pub sm_map: NoditMap<u32, Interval<u32>, StorageModule>,
}

impl StorageProvider {
    /// read a set of chunks using their partition-relative offsets - **start is inclusive, end is exclusive**, so (0, 1) will read just chunk 0, and (3, 5) will read chunks 3 and 4
    pub fn read_chunks(
        &mut self,
        start: u32,
        end: u32,
        expected_state: Option<PackingState>,
    ) -> eyre::Result<Vec<[u8; CHUNK_SIZE as usize]>> {
        // figure out what SMs we need
        let read_interval = ie(start, end); // intervals are interally represented as inclusive-inclusive
        let sm_iter = self.sm_map.overlapping(read_interval);
        // TODO: read in parallel, probably use streams?
        // also use a read-handle pool (removes opening delay), with a dedicated read handle for mining w/ a higher prio
        let mut result: Vec<[u8; CHUNK_SIZE as usize]> =
            Vec::with_capacity(read_interval.width() as usize);
        for (interval, sm) in sm_iter {
            // storage modules use relative 0 based offsets, as that's what works well with files
            let sm_offset = interval.start();

            // start reading from either the interval start (always 0) or the read interval start, depending on which is greater
            let sm_relative_start_offset = interval.start().max(read_interval.start()) - sm_offset;

            // finish reading at the end of the SM, or the read interval end, whichever is lesser
            let sm_relative_end_offset: u32 = interval.end().min(read_interval.end()) - sm_offset;

            let mut sm_read = sm.read_chunks(
                ii(sm_relative_start_offset, sm_relative_end_offset),
                expected_state,
            )?;
            result.append(&mut sm_read)
        }
        return Ok(result);
    }
}

// #[test]
// fn basic_storage_provider_test() -> eyre::Result<()> {
//     let chunks: u8 = 1;
//     generate_chunk_test_data("/tmp/sample".into(), chunks, 0)?;
//     let sm1_interval_map = NoditMap::from_slice_strict([(
//         ie(0, chunks as u32),
//         IntervalStateWrapped::new(IntervalState {
//             state: PackingState::Data,
//         }),
//     )])
//     .unwrap();

//     let mut sp = StorageProvider {
//         sm_map: NoditMap::from_slice_strict([(
//             ie(0, chunks as u32),
//             StorageModule {
//                 path: "/tmp/sample".into(),
//                 interval_map: RwLock::new(sm1_interval_map),
//                 capacity: chunks as u32,
//             },
//         )])
//         .unwrap(),
//     };

//     let res = sp.read_chunks(0, 1)?;
//     dbg!(res);
//     Ok(())
// }

// #[test]
// fn basic_storage_provider_test2() -> eyre::Result<()> {
//     let chunks1: u8 = 4;
//     generate_chunk_test_data("/tmp/sample1".into(), chunks1, 0)?;

//     let sm1_interval_map = NoditMap::from_slice_strict([(
//         ie(0, 4),
//         IntervalStateWrapped::new(IntervalState {
//             state: PackingState::Data,
//         }),
//     )])
//     .unwrap();

//     let chunks2: u8 = 4;
//     generate_chunk_test_data("/tmp/sample2".into(), chunks2, 4)?;
//     let sm2_interval_map = NoditMap::from_slice_strict([(
//         ie(4, 8),
//         IntervalStateWrapped::new(IntervalState {
//             state: PackingState::Data,
//         }),
//     )])
//     .unwrap();

//     let mut sp = StorageProvider {
//         sm_map: NoditMap::from_slice_strict([
//             (
//                 ie(0, 4),
//                 StorageModule {
//                     path: "/tmp/sample1".into(),
//                     interval_map: RwLock::new(sm1_interval_map),
//                     capacity: 5,
//                 },
//             ),
//             (
//                 ie(4, 8),
//                 StorageModule {
//                     path: "/tmp/sample2".into(),
//                     interval_map: RwLock::new(sm2_interval_map),
//                     capacity: 5,
//                 },
//             ),
//         ])
//         .unwrap(),
//     };

//     let res = sp.read_chunks(3, 5)?;

//     dbg!(res);
//     Ok(())
// }

/// Writes N random chunks to the specified path, filled with a specific value (count + offset) so you know if the correct chunk(s) are being read
pub fn generate_chunk_test_data(path: PathBuf, chunks: u8, offset: u8) -> eyre::Result<()> {
    let mut chunks_done = 0;
    let mut handle = File::create(path)?;
    // let mut rng = rand::thread_rng();
    // let mut chunk = [0; CHUNK_SIZE as usize];
    while chunks_done < chunks {
        // rng.fill_bytes(&mut chunk);
        // handle.write(&chunk)?;

        handle.write([offset + chunks_done as u8; CHUNK_SIZE as usize].as_ref())?;

        chunks_done = chunks_done + 1;
    }
    Ok(())
}

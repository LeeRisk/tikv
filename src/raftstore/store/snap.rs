use std::io::{self, Write, ErrorKind, Read};
use std::fmt::{self, Formatter, Display};
use std::fs::{self, File, OpenOptions, Metadata};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, RwLock};
use std::path::{Path, PathBuf};

use protobuf::Message;

use kvproto::eraftpb::Snapshot;
use kvproto::raft_serverpb::RaftSnapshotData;
use raftstore::store::Msg;
use util::transport::SendCh;
use util::HandyRwLock;
use storage::{ALL_CFS, CF_RAFT};
use rocksdb::{EnvOptions, Options, SstFileWriter};

const TMP_FILE_SUFFIX: &'static str = ".tmp";
const SST_FILE_SUFFIX: &'static str = ".sst";

fn file_exists(file: &str) -> bool {
    let path = Path::new(file);
    path.exists() && path.is_file()
}

fn delete_file(file: &str) -> bool {
    if let Err(e) = fs::remove_file(file) {
        warn!("failed to delete file {}: {:?}", file, e);
        return false;
    }
    true
}

#[derive(Clone, Hash, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct SnapKey {
    pub region_id: u64,
    pub term: u64,
    pub idx: u64,
}

impl SnapKey {
    #[inline]
    pub fn new(region_id: u64, term: u64, idx: u64) -> SnapKey {
        SnapKey {
            region_id: region_id,
            term: term,
            idx: idx,
        }
    }

    #[inline]
    pub fn from_region_snap(region_id: u64, snap: &Snapshot) -> SnapKey {
        let index = snap.get_metadata().get_index();
        let term = snap.get_metadata().get_term();
        SnapKey::new(region_id, term, index)
    }

    pub fn from_snap(snap: &Snapshot) -> io::Result<SnapKey> {
        let mut snap_data = RaftSnapshotData::new();
        if let Err(e) = snap_data.merge_from_bytes(snap.get_data()) {
            return Err(io::Error::new(ErrorKind::Other, e));
        }

        Ok(SnapKey::from_region_snap(snap_data.get_region().get_id(), snap))
    }
}

impl Display for SnapKey {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}_{}_{}", self.region_id, self.term, self.idx)
    }
}

/// Name prefix for the self-generated snapshot file.
const SNAP_GEN_PREFIX: &'static str = "gen";
/// Name prefix for the received snapshot file.
const SNAP_REV_PREFIX: &'static str = "rev";

struct CfFile {
    pub cf: String,
    pub writer: SstFileWriter,
    pub path: String,
    pub tmp_path: String,
    pub size: u64,
}

pub struct SendSnapshotFile {
    dir: PathBuf,
    prefix: String,
    index: usize,
    cf_files: Vec<CfFile>,
    size_track: Arc<RwLock<u64>>,
}

impl SendSnapshotFile {
    fn new<T: Into<PathBuf>>(snap_dir: T,
                             key: &SnapKey,
                             size_track: Arc<RwLock<u64>>)
                             -> io::Result<SendSnapshotFile> {
        let dir_path = snap_dir.into();
        if !dir_path.exists() {
            try!(fs::create_dir_all(dir_path.as_path()));
        }

        let prefix = format!("{}_{}", SNAP_GEN_PREFIX, key);
        let cf_number = ALL_CFS.len() - 1;

        let mut f = SendSnapshotFile {
            dir: dir_path,
            prefix: prefix,
            index: 0,
            cf_files: Vec::with_capacity(cf_number),
            size_track: size_track,
        };
        try!(f.init());
        Ok(f)
    }

    fn init(&mut self) -> io::Result<()> {
        for cf in ALL_CFS {
            if *cf == CF_RAFT {
                continue;
            }
            // The cf raft is not needed
            try!(self.add_file(cf.to_string()));
        }
        Ok(())
    }

    fn add_file(&mut self, cf: String) -> io::Result<()> {
        let filename = format!("{}_{}{}", self.prefix, cf, SST_FILE_SUFFIX);
        let path = self.dir.join(filename).as_path().to_str().unwrap().to_string();
        let tmp_filename = format!("{}_{}{}", self.prefix, cf, TMP_FILE_SUFFIX);
        let tmp_path = self.dir.join(tmp_filename).as_path().to_str().unwrap().to_string();
        let env_opt = EnvOptions::new();
        let io_options = Options::new();
        let mut writer = SstFileWriter::new(&env_opt, &io_options);
        if let Err(e) = writer.open(&tmp_path) {
            return Err(io::Error::new(ErrorKind::Other, e));
        }
        let cf_file = CfFile {
            cf: cf,
            writer: writer,
            tmp_path: tmp_path,
            path: path,
            size: 0,
        };

        self.cf_files.push(cf_file);
        Ok(())
    }

    pub fn next_file(&mut self, cf: String) -> bool {
        let mut cf_found = false;
        let mut next_index = 0;
        for f in &mut self.cf_files {
            if f.cf == cf {
                cf_found = true;
                break;
            } else {
                next_index += 1;
            }
        }
        if !cf_found {
            return false;
        }

        println!("next file cf: {}", cf);

        self.index = next_index;
        true
    }

    pub fn add_kv(&mut self, k: &[u8], v: &[u8]) -> io::Result<()> {
        if let Err(e) = self.cf_files[self.index].writer.add(k, v) {
            return Err(io::Error::new(ErrorKind::Other, e));
        }
        Ok(())
    }

    pub fn save_all(&mut self) -> io::Result<()> {
        for f in &mut self.cf_files {
            if let Err(e) = f.writer.finish() {
                return Err(io::Error::new(ErrorKind::Other, e));
            }
            try!(fs::rename(&f.tmp_path, &f.path));
            f.size = try!(fs::metadata(&f.path)).len();
            println!("save_all cf: {}, size: {}", f.cf, f.size);
            let mut size_track = self.size_track.wl();
            *size_track = size_track.saturating_add(f.size);
        }
        Ok(())
    }

    pub fn list_cf_sizes(&self) -> Vec<(String, u64)> {
        let mut res = Vec::with_capacity(self.cf_files.len());
        for f in &self.cf_files {
            res.push((f.cf.clone(), f.size));
        }
        res
    }

    pub fn exists(&self) -> bool {
        for f in &self.cf_files {
            if !file_exists(&f.path) {
                return false;
            }
        }
        true
    }

    pub fn total_size(&self) -> u64 {
        let mut sum = 0;
        for f in &self.cf_files {
            sum += f.size;
        }
        println!("total_size: {}", sum);
        sum
    }
}

struct CfReader {
    pub cf: String,
    pub file: File,
    pub path: String,
}

pub struct SnapshotFileReader {
    dir: PathBuf,
    prefix: String,
    index: usize,
    cf_readers: Vec<CfReader>,
    size_track: Arc<RwLock<u64>>,
}

impl SnapshotFileReader {
    fn new<T: Into<PathBuf>>(snap_dir: T,
                             key: &SnapKey,
                             size_track: Arc<RwLock<u64>>)
                             -> io::Result<SnapshotFileReader> {
        let dir_path = snap_dir.into();
        let prefix = format!("{}_{}", SNAP_GEN_PREFIX, key);
        let mut cf_readers = Vec::with_capacity(ALL_CFS.len() - 1);
        for cf in ALL_CFS {
            if *cf == CF_RAFT {
                continue;
            }
            let filename = format!("{}_{}{}", prefix, cf, SST_FILE_SUFFIX);
            let path = dir_path.join(filename).as_path().to_str().unwrap().to_string();
            if !file_exists(&path) {
                return Err(io::Error::new(ErrorKind::NotFound,
                                          format!("snapshot for {} not found", key)));
            }
            let f = try!(OpenOptions::new().read(true).open(&path));
            let r = CfReader {
                cf: String::from(*cf),
                file: f,
                path: path,
            };
            cf_readers.push(r);
        }
        Ok(SnapshotFileReader {
            dir: dir_path,
            prefix: prefix,
            index: 0,
            cf_readers: cf_readers,
            size_track: size_track,
        })
    }

    pub fn exists(&self) -> bool {
        for f in &self.cf_readers {
            if !file_exists(&f.path) {
                return false;
            }
        }
        true
    }

    pub fn display_path(&self) -> String {
        let mut cf_names = String::from("");
        let mut i = 0;
        for f in &self.cf_readers {
            if i == 0 {
                cf_names += "(";
                cf_names += &f.cf;
            } else {
                cf_names += "|";
                cf_names += &f.cf;
            }
            i += 0;
        }
        cf_names += ")";
        format!("{:?}/{}_{}.{}",
                self.dir,
                self.prefix,
                cf_names,
                SST_FILE_SUFFIX)
    }

    pub fn list_cf_files(&self) -> Vec<(String, String)> {
        let mut res = Vec::with_capacity(self.cf_readers.len());
        for r in &self.cf_readers {
            res.push((r.cf.clone(), r.path.clone()));
        }
        res
    }

    pub fn list_file_paths(&self) -> Vec<String> {
        let mut res = Vec::with_capacity(self.cf_readers.len());
        for r in &self.cf_readers {
            res.push(r.path.clone());
        }
        res
    }

    pub fn delete(&self) {
        if let Err(e) = self.try_delete() {
            error!("failed to delete snapshot file {}: {:?}",
                   self.display_path(),
                   e);
        }
    }

    pub fn try_delete(&self) -> io::Result<()> {
        debug!("deleting {}", self.display_path());
        for f in &self.cf_readers {
            let size = try!(fs::metadata(&f.path)).len();
            let mut size_track = self.size_track.wl();
            try!(fs::remove_file(&f.path));
            *size_track = size_track.saturating_sub(size);
            println!("reader delete cf: {}, size: {} after delete size_track {}",
                     f.cf,
                     size,
                     *size_track);
        }
        Ok(())
    }

    pub fn meta(&self) -> io::Result<Metadata> {
        let f = try!(OpenOptions::new().open(&self.cf_readers[self.index].path));
        f.metadata()
    }

    pub fn total_size(&self) -> io::Result<u64> {
        let mut total_size = 0;
        for f in &self.cf_readers {
            total_size += try!(fs::metadata(&f.path)).len();
        }
        Ok(total_size)
    }
}

impl Read for SnapshotFileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.len() == 0 {
            return Ok(0);
        }
        while self.index < self.cf_readers.len() {
            match self.cf_readers[self.index].file.read(buf) {
                Ok(n) => {
                    if n == 0 {
                        // EOF. switch to next file
                        self.index += 1;
                        continue;
                    }
                    return Ok(n);
                }
                e => return e,
            }
        }
        Ok(0)
    }
}

struct RecvCfFile {
    pub cf: String,
    pub file: File,
    pub path: String,
    pub tmp_path: String,
    pub size: u64,
    pub written_size: u64,
}

pub struct RecvSnapshotFile {
    dir: PathBuf,
    prefix: String,
    index: usize,
    cf_files: Vec<RecvCfFile>,
    size_track: Arc<RwLock<u64>>,
}

impl RecvSnapshotFile {
    fn new<T: Into<PathBuf>>(snap_dir: T,
                             key: &SnapKey,
                             cf_sizes: Vec<(String, u64)>,
                             size_track: Arc<RwLock<u64>>)
                             -> io::Result<RecvSnapshotFile> {
        let dir_path = snap_dir.into();
        if !dir_path.exists() {
            try!(fs::create_dir_all(dir_path.as_path()));
        }

        let prefix = format!("{}_{}", SNAP_REV_PREFIX, key);

        let mut cf_files = Vec::with_capacity(cf_sizes.len());
        for (cf, size) in cf_sizes {
            let tmp_filename = format!("{}_{}{}", prefix, cf, TMP_FILE_SUFFIX);
            let tmp_path = dir_path.join(tmp_filename).as_path().to_str().unwrap().to_string();
            let filename = format!("{}_{}{}", prefix, cf, SST_FILE_SUFFIX);
            let path = dir_path.join(filename).as_path().to_str().unwrap().to_string();
            let f = try!(OpenOptions::new().write(true).create_new(true).open(&tmp_path));
            let cf_file = RecvCfFile {
                cf: cf,
                file: f,
                tmp_path: tmp_path,
                path: path,
                size: size,
                written_size: 0,
            };
            cf_files.push(cf_file);
        }
        Ok(RecvSnapshotFile {
            dir: dir_path,
            prefix: prefix,
            index: 0,
            cf_files: cf_files,
            size_track: size_track,
        })
    }

    pub fn exists(&self) -> bool {
        for f in &self.cf_files {
            if !file_exists(&f.path) {
                return false;
            }
        }
        true
    }

    pub fn display_path(&self) -> String {
        let mut cf_names = String::from("");
        let mut i = 0;
        for f in &self.cf_files {
            if i == 0 {
                cf_names += "(";
                cf_names += &f.cf;
            } else {
                cf_names += "|";
                cf_names += &f.cf;
            }
            i += 0;
        }
        cf_names += ")";
        format!("{:?}/{}_{}.{}",
                self.dir,
                self.prefix,
                cf_names,
                SST_FILE_SUFFIX)
    }

    pub fn save(&mut self) -> io::Result<()> {
        debug!("saving to {}", self.display_path());
        for cf_file in &mut self.cf_files {
            try!(cf_file.file.flush());
            let mut size_track = self.size_track.wl();
            try!(fs::rename(&cf_file.tmp_path, &cf_file.path));
            *size_track = size_track.saturating_add(cf_file.size as u64);
            println!("recv save cf: {}, size: {}", cf_file.cf, cf_file.size);
        }
        Ok(())
    }
}

impl Write for RecvSnapshotFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut next_buf = buf;
        while self.index < self.cf_files.len() {
            let cf_file = &mut self.cf_files[self.index];
            let left = (cf_file.size - cf_file.written_size) as usize;
            if next_buf.len() > left {
                try!(cf_file.file.write_all(&next_buf[0..left]));
                cf_file.written_size += left as u64;
                self.index += 1;
                next_buf = &next_buf[left..];
            } else {
                try!(cf_file.file.write_all(next_buf));
                cf_file.written_size = next_buf.len() as u64;
                return Ok(buf.len());
            }
        }
        let n = buf.len() - next_buf.len();
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        for cf_file in &mut self.cf_files {
            try!(cf_file.file.flush());
        }
        Ok(())
    }
}

impl Drop for RecvSnapshotFile {
    fn drop(&mut self) {
        debug!("deleting {}", self.display_path());
        let mut done_writing = true;
        for cf_file in &self.cf_files {
            if file_exists(&cf_file.tmp_path) {
                done_writing = false;
            }
        }
        if !done_writing {
            for cf_file in &self.cf_files {
                delete_file(&cf_file.tmp_path);
                delete_file(&cf_file.path);
            }
        }
    }
}

#[derive(PartialEq, Debug)]
pub enum SnapEntry {
    Generating = 1,
    Sending = 2,
    Receiving = 3,
    Applying = 4,
}

/// `SnapStats` is for snapshot statistics.
pub struct SnapStats {
    pub sending_count: usize,
    pub receiving_count: usize,
}

/// `SnapManagerCore` trace all current processing snapshots.
pub struct SnapManagerCore {
    // directory to store snapfile.
    base: String,
    registry: HashMap<SnapKey, Vec<SnapEntry>>,
    ch: Option<SendCh<Msg>>,
    snap_size: Arc<RwLock<u64>>,
}

impl SnapManagerCore {
    pub fn new<T: Into<String>>(path: T, ch: Option<SendCh<Msg>>) -> SnapManagerCore {
        SnapManagerCore {
            base: path.into(),
            registry: map![],
            ch: ch,
            snap_size: Arc::new(RwLock::new(0)),
        }
    }

    pub fn init(&self) -> io::Result<()> {
        let path = Path::new(&self.base);
        if !path.exists() {
            try!(fs::create_dir_all(path));
            return Ok(());
        }
        if !path.is_dir() {
            return Err(io::Error::new(ErrorKind::Other,
                                      format!("{} should be a directory", path.display())));
        }
        let mut size = self.snap_size.wl();
        for f in try!(fs::read_dir(path)) {
            let p = try!(f);
            if try!(p.file_type()).is_file() {
                if let Some(s) = p.file_name().to_str() {
                    if s.ends_with(TMP_FILE_SUFFIX) {
                        try!(fs::remove_file(p.path()));
                    } else {
                        *size += try!(p.metadata()).len();
                    }
                }
            }
        }
        Ok(())
    }

    pub fn list_snap(&self) -> io::Result<Vec<(SnapKey, bool)>> {
        let path = Path::new(&self.base);
        let read_dir = try!(fs::read_dir(path));
        Ok(read_dir.filter_map(|p| {
                let p = match p {
                    Err(e) => {
                        error!("failed to list content of {}: {:?}", self.base, e);
                        return None;
                    }
                    Ok(p) => p,
                };
                match p.file_type() {
                    Ok(t) if t.is_file() => {}
                    _ => return None,
                }
                let file_name = p.file_name();
                let name = match file_name.to_str() {
                    None => return None,
                    Some(n) => n,
                };
                let is_sending = name.starts_with(SNAP_GEN_PREFIX);
                let numbers: Vec<u64> = name.split('.')
                    .next()
                    .map_or_else(|| vec![], |s| {
                        s.split('_')
                            .skip(1)
                            .filter_map(|s| s.parse().ok())
                            .collect()
                    });
                if numbers.len() != 3 {
                    error!("failed to parse snapkey from {}", name);
                    return None;
                }
                Some((SnapKey::new(numbers[0], numbers[1], numbers[2]), is_sending))
            })
            .collect())
    }

    #[inline]
    pub fn has_registered(&self, key: &SnapKey) -> bool {
        self.registry.contains_key(key)
    }

    #[inline]
    pub fn get_send_snap_file(&self, key: &SnapKey) -> io::Result<SendSnapshotFile> {
        SendSnapshotFile::new(&self.base, key, self.snap_size.clone())
    }

    #[inline]
    pub fn get_recv_snap_file(&self,
                              key: &SnapKey,
                              cf_sizes: Vec<(String, u64)>)
                              -> io::Result<RecvSnapshotFile> {
        RecvSnapshotFile::new(&self.base, key, cf_sizes, self.snap_size.clone())
    }

    #[inline]
    pub fn get_snap_file_reader(&self, key: &SnapKey) -> io::Result<SnapshotFileReader> {
        SnapshotFileReader::new(&self.base, key, self.snap_size.clone())
    }

    /// Get the approximate size of snap file exists in snap directory.
    ///
    /// Return value is not guaranteed to be accurate.
    pub fn get_total_snap_size(&self) -> u64 {
        *self.snap_size.rl()
    }

    pub fn register(&mut self, key: SnapKey, entry: SnapEntry) {
        debug!("register [key: {}, entry: {:?}]", key, entry);
        match self.registry.entry(key) {
            Entry::Occupied(mut e) => {
                if e.get().contains(&entry) {
                    warn!("{} is registered more than 1 time!!!", e.key());
                    return;
                }
                e.get_mut().push(entry);
            }
            Entry::Vacant(e) => {
                e.insert(vec![entry]);
            }
        }

        self.notify_stats();
    }

    pub fn deregister(&mut self, key: &SnapKey, entry: &SnapEntry) {
        debug!("deregister [key: {}, entry: {:?}]", key, entry);
        let mut need_clean = false;
        let mut handled = false;
        if let Some(e) = self.registry.get_mut(key) {
            let last_len = e.len();
            e.retain(|e| e != entry);
            need_clean = e.is_empty();
            handled = last_len > e.len();
        }
        if need_clean {
            self.registry.remove(key);
        }
        if handled {
            self.notify_stats();
            return;
        }
        warn!("stale deregister key: {} {:?}", key, entry);
    }

    fn notify_stats(&self) {
        if let Some(ref ch) = self.ch {
            if let Err(e) = ch.try_send(Msg::SnapshotStats) {
                error!("notify snapshot stats failed {:?}", e)
            }
        }
    }

    pub fn stats(&self) -> SnapStats {
        // send_count, generating_count, receiving_count, applying_count
        let (mut sending_cnt, mut receiving_cnt) = (0, 0);
        for v in self.registry.values() {
            let (mut is_sending, mut is_receiving) = (false, false);
            for s in v {
                match *s {
                    SnapEntry::Sending | SnapEntry::Generating => is_sending = true,
                    SnapEntry::Receiving | SnapEntry::Applying => is_receiving = true,
                }
            }
            if is_sending {
                sending_cnt += 1;
            }
            if is_receiving {
                receiving_cnt += 1;
            }
        }

        SnapStats {
            sending_count: sending_cnt,
            receiving_count: receiving_cnt,
        }
    }
}

pub type SnapManager = Arc<RwLock<SnapManagerCore>>;

pub fn new_snap_mgr<T: Into<String>>(path: T, ch: Option<SendCh<Msg>>) -> SnapManager {
    Arc::new(RwLock::new(SnapManagerCore::new(path, ch)))
}

#[cfg(test)]
mod test {
    use std::path::Path;
    use std::fs::{File, OpenOptions};
    use std::io::*;
    use std::sync::*;

    use tempdir::TempDir;

    use util::HandyRwLock;
    use super::*;

    const BUF_LEN: usize = 10000;

    fn write_test_snapshot_file(f: &mut SendSnapshotFile) {
        // Write at least one key-value to the SendSnapshotFile
        // because it's not allowed to finish a rocksdb sst file writer with no entries.
        for cf in ALL_CFS {
            if !f.next_file(cf.to_string()) {
                continue;
            }
            f.add_kv("k".as_bytes(), "v".as_bytes()).unwrap();
        }
    }

    #[test]
    fn test_snap_mgr() {
        let path = TempDir::new("test-snap-mgr").unwrap();

        // `mgr` should create the specified directory when it does not exist.
        let path1 = path.path().to_str().unwrap().to_owned() + "/snap1";
        let p = Path::new(&path1);
        assert!(!p.exists());
        let mut mgr = new_snap_mgr(path1.clone(), None);
        mgr.wl().init().unwrap();
        assert!(p.exists());

        // if target is a file, an error should be returned.
        let path2 = path.path().to_str().unwrap().to_owned() + "/snap2";
        File::create(&path2).unwrap();
        mgr = new_snap_mgr(path2, None);
        assert!(mgr.wl().init().is_err());

        // if temporary files exist, they should be deleted.
        let path3 = path.path().to_str().unwrap().to_owned() + "/snap3";
        let key1 = SnapKey::new(1, 1, 1);
        let size_track = Arc::new(RwLock::new(0));
        let f1 = SendSnapshotFile::new(&path3, &key1, size_track.clone()).unwrap();
        let f2 = SendSnapshotFile::new(&path3, &key1, size_track.clone()).unwrap();
        let key2 = SnapKey::new(2, 1, 1);
        let mut f3 = SendSnapshotFile::new(&path3, &key2, size_track.clone()).unwrap();
        write_test_snapshot_file(&mut f3);
        f3.save_all().unwrap();
        let mut f4 = SendSnapshotFile::new(&path3, &key2, size_track.clone()).unwrap();
        write_test_snapshot_file(&mut f4);
        f4.save_all().unwrap();
        assert!(!f1.exists());
        assert!(!f2.exists());
        assert!(f3.exists());
        assert!(f4.exists());
        mgr = new_snap_mgr(path3, None);
        mgr.wl().init().unwrap();
        assert!(!f1.exists());
        assert!(!f2.exists());
        assert!(f3.exists());
        assert!(f4.exists());
    }

    #[test]
    fn test_snap_size() {
        let path = TempDir::new("test-snap-mgr").unwrap();
        let path_str = path.path().to_str().unwrap();
        let mut mgr = new_snap_mgr(path_str, None);
        mgr.wl().init().unwrap();
        assert_eq!(mgr.rl().get_total_snap_size(), 0);

        let snap_key1 = SnapKey::new(1, 1, 1);
        let size_track = Arc::new(RwLock::new(0));
        let mut f1 = SendSnapshotFile::new(path_str, &snap_key1, size_track.clone()).unwrap();
        write_test_snapshot_file(&mut f1);
        f1.save_all().unwrap();
        let cf_sizes = f1.list_cf_sizes();

        println!("f1 save_all");
        let total_size = f1.total_size();
        let mut reader = SnapshotFileReader::new(path_str, &snap_key1, size_track.clone()).unwrap();
        let mut buf = Vec::with_capacity(BUF_LEN);
        assert!(total_size < BUF_LEN as u64);
        let read_size = reader.read_to_end(&mut buf).unwrap();
        assert_eq!(read_size as u64, total_size);

        println!("reader read");
        let snap_key2 = SnapKey::new(2, 1, 1);
        let mut f2 = RecvSnapshotFile::new(path_str, &snap_key2, cf_sizes, size_track.clone())
            .unwrap();
        f2.write_all(&buf[0..read_size]).unwrap();
        f2.save().unwrap();
        println!("f2 save");

        let snap_key3 = SnapKey::new(3, 1, 1);
        let mut f3 = SendSnapshotFile::new(path_str, &snap_key3, size_track.clone()).unwrap();
        write_test_snapshot_file(&mut f3);

        mgr = new_snap_mgr(path_str, None);
        mgr.wl().init().unwrap();
        // temporary file should not be count in snap size.
        assert_eq!(mgr.rl().get_total_snap_size(), total_size * 2);

        reader.delete();
        println!("xxxxx");
        assert_eq!(mgr.rl().get_total_snap_size(), total_size);
        mgr.rl().get_snap_file_reader(&snap_key2).unwrap().delete();
        assert_eq!(mgr.rl().get_total_snap_size(), 0);

        let snap_key4 = SnapKey::new(4, 1, 1);
        let mut f4 = mgr.rl().get_send_snap_file(&snap_key4).unwrap();
        write_test_snapshot_file(&mut f4);
        assert_eq!(mgr.rl().get_total_snap_size(), 0);
        f4.save_all().unwrap();
        let size = f4.total_size();
        assert_eq!(mgr.rl().get_total_snap_size(), size);
    }
}

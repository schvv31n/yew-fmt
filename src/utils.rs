use anyhow::{anyhow, Context, Result};
use std::{
    fs::{write, File},
    io::{self, Read, Seek, Write},
    ops::Deref,
    path::Path,
    str::FromStr,
};

pub trait StrExt {
    /// Returns the length of the last line of the string, or None if the string has only 1 line
    fn last_line_len(&self) -> Option<usize>;
    /// Unchecked version of `split_at`, caller must ensure that `self.is_char_boundary(mid)`
    unsafe fn split_at_unchecked(&self, mid: usize) -> (&str, &str);
    /// Non-panicking version of `split_at`
    fn try_split_at(&self, mid: usize) -> Option<(&str, &str)>;
}

impl StrExt for str {
    fn last_line_len(&self) -> Option<usize> {
        self.bytes()
            .rev()
            .enumerate()
            .find_map(|(i, c)| (c == b'\n').then_some(i))
    }

    unsafe fn split_at_unchecked(&self, mid: usize) -> (&str, &str) {
        (self.get_unchecked(..mid), self.get_unchecked(mid..))
    }

    fn try_split_at(&self, mid: usize) -> Option<(&str, &str)> {
        self.is_char_boundary(mid).then(|| unsafe {
            // SAFETY: just checked that `mid` is on a char boundary.
            (self.get_unchecked(..mid), self.get_unchecked(mid..))
        })
    }
}

#[derive(Clone)]
#[repr(transparent)]
pub struct KVPairs(Box<[(Box<str>, Box<str>)]>);

impl Deref for KVPairs {
    type Target = [(Box<str>, Box<str>)];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FromStr for KVPairs {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Ok(Self(Box::from([])));
        }
        s.split(',')
            .map(|p| {
                p.split_once('=')
                    .map(|(k, v)| (k.into(), v.into()))
                    .ok_or(p)
            })
            .collect::<Result<_, _>>()
            .map_err(|p| anyhow!("invalid key=val pair: `{p}`"))
            .map(Self)
    }
}

/// like `std::fs::write`, but will also create a `.bk` file
pub fn write_with_backup(filename: &str, new_text: impl AsRef<[u8]>) -> Result<()> {
    let new_text = new_text.as_ref();
    let mut file = File::options()
        .read(true)
        .write(true)
        .open(filename)
        .context("failed to open the file")?;
    let mut old_text = vec![];
    file.read_to_end(&mut old_text)
        .context("failed to read the file")?;
    Ok(if &old_text[..] != new_text {
        let backup = Path::new(filename).with_extension("bk");
        write(&backup, old_text)
            .with_context(|| format!("failed to create a backup file {:?}", backup.as_os_str()))?;
        file.rewind().context("failed to rewind the file handle")?;
        file.set_len(0).context("failed to clear the file")?;
        file.write_all(new_text)
            .context("failed to write new data to the file")?;
    })
}

/// like `fs::read`, but allows for reusing allocations
pub fn read_into(file: impl AsRef<Path>, dst: &mut Vec<u8>) -> io::Result<()> {
    dst.clear();
    File::open(file)?.read_to_end(dst).map(drop)
}

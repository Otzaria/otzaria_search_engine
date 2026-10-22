use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tantivy::schema::document::ReferenceValueLeaf;
use tantivy::schema::{Facet, Field, Schema, Value};
use tantivy::TantivyDocument;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PreparationPolicy {
    pub book_workers: usize,
    pub cpu_threads: usize,
    pub text_bytes: usize,
    pub document_bytes: usize,
    pub window_lines: usize,
    pub window_bytes: usize,
}

impl PreparationPolicy {
    pub(crate) fn new(economy: bool, cores: usize) -> Self {
        if economy {
            Self {
                book_workers: 1,
                cpu_threads: 1,
                text_bytes: 1 << 20,
                document_bytes: 2 << 20,
                window_lines: 256,
                window_bytes: 1 << 20,
            }
        } else {
            Self {
                book_workers: 4,
                cpu_threads: cores.max(1),
                text_bytes: 32 << 20,
                document_bytes: 64 << 20,
                window_lines: 16_384,
                window_bytes: 16 << 20,
            }
        }
    }

    pub(crate) fn reservation(self) -> usize {
        self.text_bytes + self.document_bytes
    }
}

pub(crate) fn preparation_cpu<T: Send>(
    economy: bool,
    prepare: impl FnOnce() -> T + Send,
) -> Result<T> {
    if economy {
        static POOL: once_cell::sync::OnceCell<rayon::ThreadPool> =
            once_cell::sync::OnceCell::new();
        let pool =
            POOL.get_or_try_init(|| rayon::ThreadPoolBuilder::new().num_threads(1).build())?;
        Ok(pool.install(prepare))
    } else {
        Ok(prepare())
    }
}

const SPOOL_PREFIX: &str = ".otzaria-prepare-";

/// Call only after acquiring this directory's index-writer lock.
pub(crate) fn cleanup_abandoned(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(SPOOL_PREFIX))
            && entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            if let Err(error) = std::fs::remove_file(entry.path()) {
                log::debug!("cannot remove an abandoned preparation spool: {error}");
            }
        }
    }
}

/// Spills before a whole book can accumulate in the process heap.
#[derive(Debug)]
pub(crate) struct TextBuffer {
    memory: Vec<u8>,
    file: Option<BufWriter<tempfile::NamedTempFile>>,
    limit: usize,
    len: u64,
    skip: u64,
    tracked: crate::indexing_diagnostics::BufferedBytes,
    directory: Option<PathBuf>,
    fingerprints: Option<[u64; 2]>,
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new(1 << 20)
    }
}

impl TextBuffer {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            memory: Vec::new(),
            file: None,
            limit,
            len: 0,
            skip: 0,
            tracked: Default::default(),
            directory: None,
            fingerprints: None,
        }
    }

    pub(crate) fn in_directory(limit: usize, directory: Option<&Path>) -> Self {
        Self {
            directory: directory.map(Path::to_path_buf),
            ..Self::new(limit)
        }
    }

    pub(crate) fn fingerprinting() -> Self {
        Self {
            fingerprints: Some([0xcbf2_9ce4_8422_2325; 2]),
            ..Self::new(0)
        }
    }

    pub(crate) fn fingerprint(&self) -> Result<u64> {
        let hashes = self
            .fingerprints
            .context("the buffer is not in fingerprint-only mode")?;
        let hash = match self.skip {
            0 => hashes[0],
            3 => hashes[1],
            _ => anyhow::bail!("unexpected fingerprint prefix"),
        };
        Ok(hash.max(1))
    }

    pub(crate) fn push_str(&mut self, text: &str) -> Result<()> {
        self.push_bytes(text.as_bytes())
    }

    fn push_bytes(&mut self, text: &[u8]) -> Result<()> {
        if let Some(hashes) = &mut self.fingerprints {
            for (offset, &byte) in text.iter().enumerate() {
                hashes[0] = (hashes[0] ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
                if self.len + offset as u64 >= 3 {
                    hashes[1] = (hashes[1] ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            self.len += text.len() as u64;
            return Ok(());
        }
        if self.file.is_none() && self.memory.len().saturating_add(text.len()) > self.limit {
            let mut file =
                BufWriter::with_capacity(64 << 10, temporary_file(self.directory.as_deref())?);
            file.write_all(&self.memory)?;
            self.memory = Vec::new();
            self.tracked.set(64 << 10);
            crate::indexing_diagnostics::spilled(self.len);
            self.file = Some(file);
        }
        if let Some(file) = &mut self.file {
            file.write_all(text)?;
            crate::indexing_diagnostics::spilled(text.len() as u64);
        } else {
            let needed = self.memory.len() + text.len();
            if needed > self.memory.capacity() {
                let capacity = needed
                    .max(self.memory.capacity().saturating_mul(2))
                    .min(self.limit);
                self.memory
                    .try_reserve_exact(capacity - self.memory.len())?;
            }
            self.memory.extend_from_slice(text);
            self.tracked.set(self.memory.capacity());
        }
        self.len += text.len() as u64;
        Ok(())
    }

    pub(crate) fn skip_prefix(&mut self, bytes: u64) {
        self.skip = bytes.min(self.len);
    }

    pub(crate) fn len(&self) -> u64 {
        self.len - self.skip
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn as_str(&self) -> Result<Option<&str>> {
        if self.file.is_some() || self.fingerprints.is_some() {
            return Ok(None);
        }
        Ok(Some(std::str::from_utf8(
            &self.memory[self.skip as usize..],
        )?))
    }

    pub(crate) fn reader(&mut self) -> Result<Box<dyn BufRead + '_>> {
        anyhow::ensure!(
            self.fingerprints.is_none(),
            "a fingerprint-only buffer has no stored text"
        );
        if let Some(file) = &mut self.file {
            file.flush()?;
            file.get_mut()
                .as_file_mut()
                .seek(SeekFrom::Start(self.skip))?;
            Ok(Box::new(BufReader::with_capacity(
                64 << 10,
                file.get_mut().as_file_mut(),
            )))
        } else {
            Ok(Box::new(std::io::Cursor::new(
                &self.memory[self.skip as usize..],
            )))
        }
    }

    #[cfg(test)]
    pub(crate) fn spilled_bytes(&self) -> u64 {
        if self.file.is_some() {
            self.len()
        } else {
            0
        }
    }

    #[cfg(test)]
    pub(crate) fn buffered_bytes(&self) -> usize {
        self.memory.capacity()
    }
}

#[derive(Debug)]
pub(crate) struct OrdinalBuffer(TextBuffer);

impl Default for OrdinalBuffer {
    fn default() -> Self {
        Self(TextBuffer::new(64 << 10))
    }
}

impl OrdinalBuffer {
    pub(crate) fn in_directory(directory: Option<&Path>) -> Self {
        Self(TextBuffer::in_directory(64 << 10, directory))
    }

    pub(crate) fn push(&mut self, ordinal: u32) -> Result<()> {
        self.0.push_bytes(&ordinal.to_le_bytes())
    }

    pub(crate) fn iter(&mut self) -> Result<impl Iterator<Item = Result<u32>> + '_> {
        let count = self.0.len() / 4;
        let mut reader = self.0.reader()?;
        Ok((0..count).map(move |_| read_u32(&mut reader)))
    }
}

pub(crate) struct DocumentSpool {
    file: BufWriter<tempfile::NamedTempFile>,
    count: u32,
}

impl DocumentSpool {
    pub(crate) fn new(_schema: &Schema, directory: &Path) -> Result<Self> {
        Ok(Self {
            file: BufWriter::with_capacity(64 << 10, temporary_file(Some(directory))?),
            count: 0,
        })
    }

    pub(crate) fn push(&mut self, document: &TantivyDocument) -> Result<()> {
        let mut bytes = 4u64;
        self.file
            .write_all(&(document.len() as u32).to_le_bytes())?;
        for (field, value) in document.field_values() {
            self.file.write_all(&field.field_id().to_le_bytes())?;
            bytes += 5;
            match value.as_leaf() {
                Some(ReferenceValueLeaf::Str(text)) => {
                    self.file.write_all(&[0])?;
                    bytes += write_string(&mut self.file, text)?;
                }
                Some(ReferenceValueLeaf::Facet(text)) => {
                    self.file.write_all(&[1])?;
                    bytes += write_string(&mut self.file, text)?;
                }
                Some(ReferenceValueLeaf::U64(value)) => {
                    self.file.write_all(&[2])?;
                    self.file.write_all(&value.to_le_bytes())?;
                    bytes += 8;
                }
                Some(ReferenceValueLeaf::Bool(value)) => {
                    self.file.write_all(&[3, u8::from(value)])?;
                    bytes += 1;
                }
                other => anyhow::bail!("unsupported prepared text-book field: {other:?}"),
            }
        }
        self.count += 1;
        crate::indexing_diagnostics::spilled(bytes);
        Ok(())
    }

    pub(crate) fn for_each(
        &mut self,
        mut f: impl FnMut(TantivyDocument) -> Result<()>,
    ) -> Result<u32> {
        self.file.flush()?;
        self.file.get_mut().as_file_mut().rewind()?;
        let mut reader = BufReader::with_capacity(1 << 20, self.file.get_mut().as_file_mut());
        for _ in 0..self.count {
            let fields = read_u32(&mut reader)?;
            let mut document = TantivyDocument::new();
            for _ in 0..fields {
                let field = Field::from_field_id(read_u32(&mut reader)?);
                let mut tag = [0u8];
                reader.read_exact(&mut tag)?;
                match tag[0] {
                    0 => document.add_text(field, read_string(&mut reader)?),
                    1 => document.add_facet(
                        field,
                        Facet::from_encoded(read_string(&mut reader)?.into_bytes())?,
                    ),
                    2 => {
                        let mut bytes = [0u8; 8];
                        reader.read_exact(&mut bytes)?;
                        document.add_u64(field, u64::from_le_bytes(bytes));
                    }
                    3 => {
                        reader.read_exact(&mut tag)?;
                        document.add_bool(field, tag[0] != 0);
                    }
                    tag => anyhow::bail!("invalid prepared text-book field tag {tag}"),
                }
            }
            f(document)?;
        }
        Ok(self.count)
    }
}

fn temporary_file(directory: Option<&Path>) -> Result<tempfile::NamedTempFile> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(SPOOL_PREFIX);
    Ok(match directory {
        Some(directory) => builder.tempfile_in(directory)?,
        None => builder.tempfile()?,
    })
}

fn write_string(writer: &mut impl Write, value: &str) -> Result<u64> {
    writer.write_all(&(value.len() as u64).to_le_bytes())?;
    writer.write_all(value.as_bytes())?;
    Ok(8 + value.len() as u64)
}

fn read_u32(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_string(reader: &mut impl Read) -> Result<String> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    let mut value = vec![0u8; usize::try_from(u64::from_le_bytes(bytes))?];
    reader.read_exact(&mut value)?;
    Ok(String::from_utf8(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn economy_cpu_uses_one_reused_worker_without_changing_normal_pool() {
        let normal = preparation_cpu(false, rayon::current_num_threads).unwrap();
        for _ in 0..3 {
            assert_eq!(
                preparation_cpu(true, rayon::current_num_threads).unwrap(),
                1
            );
            assert_eq!(
                preparation_cpu(false, rayon::current_num_threads).unwrap(),
                normal
            );
        }
    }

    #[test]
    fn only_explicit_economy_limits_preparation_parallelism_and_small_buffers() {
        for cores in [1, 2, 4, 8, 64] {
            let normal = PreparationPolicy::new(false, cores);
            let economy = PreparationPolicy::new(true, cores);
            assert_eq!(normal.cpu_threads, cores);
            assert_eq!(normal.book_workers, 4);
            assert_eq!(normal.reservation() * normal.book_workers, 384 << 20);
            assert_eq!(economy.reservation(), 3 << 20);
            assert_eq!((economy.book_workers, economy.cpu_threads), (1, 1));
            assert!(normal.window_bytes > economy.window_bytes);
            assert!(normal.window_lines > economy.window_lines);
        }
    }

    #[test]
    fn normal_text_buffer_keeps_midsize_sources_in_memory() {
        let text = "אבגדה".repeat(1 << 18);
        for economy in [false, true] {
            let policy = PreparationPolicy::new(economy, 4);
            let mut buffer = TextBuffer::new(policy.text_bytes);
            buffer.push_str(&text).unwrap();
            assert_eq!(buffer.spilled_bytes() > 0, economy);
            let mut actual = String::new();
            buffer
                .reader()
                .unwrap()
                .read_to_string(&mut actual)
                .unwrap();
            assert_eq!(actual, text);
        }
    }

    #[test]
    fn text_spill_preserves_utf8_bom_and_repeated_reads() {
        let mut buffer = TextBuffer::new(4);
        buffer.push_str("\u{feff}א").unwrap();
        buffer.push_str("\nב\n").unwrap();
        buffer.skip_prefix(3);
        for _ in 0..2 {
            let mut text = String::new();
            buffer.reader().unwrap().read_to_string(&mut text).unwrap();
            assert_eq!(text, "א\nב\n");
        }
        assert_eq!(buffer.buffered_bytes(), 0);
        assert_eq!(buffer.spilled_bytes(), 6);
    }

    #[test]
    fn explicit_spool_directory_is_used_and_files_are_removed_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        {
            let mut text = TextBuffer::in_directory(1, Some(directory.path()));
            text.push_str("טקסט ארוך").unwrap();
            let mut ordinals = OrdinalBuffer::in_directory(Some(directory.path()));
            for ordinal in 0..20_000 {
                ordinals.push(ordinal).unwrap();
            }
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
            assert_eq!(
                ordinals.iter().unwrap().map(Result::unwrap).last(),
                Some(19_999)
            );
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_large_fingerprint_never_allocates_or_spills_the_book() {
        let raw = format!("\u{feff}{}", "טקסט גדול\n".repeat(200_000));
        let mut text = TextBuffer::fingerprinting();
        for fragment in raw.as_bytes().chunks(7) {
            text.push_bytes(fragment).unwrap();
        }
        assert_eq!(
            text.fingerprint().unwrap(),
            crate::api::search_engine::compute_content_fingerprint(raw.clone())
        );
        text.skip_prefix(3);
        assert_eq!(
            text.fingerprint().unwrap(),
            crate::api::search_engine::compute_content_fingerprint(raw[3..].to_string())
        );
        assert_eq!(text.buffered_bytes(), 0);
        assert_eq!(text.spilled_bytes(), 0);
        assert!(text.reader().is_err());
    }
}

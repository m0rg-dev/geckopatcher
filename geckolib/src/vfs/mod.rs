use crate::crypto::Unpackable;
use crate::iso::consts::OFFSET_DOL_OFFSET;
use crate::iso::disc::{align_addr, DiscType};
use crate::iso::read::DiscReader;
use crate::iso::{consts, FstEntry, FstNodeType};
#[cfg(feature = "progress")]
use crate::UPDATER;
use async_std::io::prelude::{ReadExt, SeekExt, WriteExt};
use async_std::io::{self, Read as AsyncRead, Seek as AsyncSeek, Write as AsyncWrite};
use async_std::sync::{Arc, Mutex};
use byteorder::{ByteOrder, BE};
use eyre::Result;
#[cfg(feature = "progress")]
use human_bytes::human_bytes;
use std::io::{Error, SeekFrom};
use std::ops::DerefMut;
#[cfg(feature = "progress")]
use std::sync::TryLockError;
use std::task::{Context, Poll};

mod directory;
pub use directory::Directory;

pub trait Node<R> {
    fn name(&self) -> &str;
    fn get_type(&self) -> NodeType;
    fn into_directory(self) -> Option<Directory<R>>;
    fn as_directory_ref(&self) -> Option<&Directory<R>>;
    fn as_directory_mut(&mut self) -> Option<&mut Directory<R>>;
    fn into_file(self) -> Option<File<R>>;
    fn as_file_ref(&self) -> Option<&File<R>>;
    fn as_file_mut(&mut self) -> Option<&mut File<R>>;

    fn as_enum_ref(&self) -> NodeEnumRef<'_, R> {
        match self.get_type() {
            NodeType::File => NodeEnumRef::File(self.as_file_ref().unwrap()),
            NodeType::Directory => NodeEnumRef::Directory(self.as_directory_ref().unwrap()),
        }
    }

    fn as_enum_mut(&mut self) -> NodeEnumMut<'_, R> {
        match self.get_type() {
            NodeType::File => NodeEnumMut::File(self.as_file_mut().unwrap()),
            NodeType::Directory => NodeEnumMut::Directory(self.as_directory_mut().unwrap()),
        }
    }

    fn into_enum(self) -> NodeEnum<R>
    where
        Self: Sized,
    {
        match self.get_type() {
            NodeType::File => NodeEnum::File(self.into_file().unwrap()),
            NodeType::Directory => NodeEnum::Directory(self.into_directory().unwrap()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub enum NodeType {
    File = 0,
    Directory,
}

pub enum NodeEnumRef<'a, R> {
    File(&'a File<R>),
    Directory(&'a Directory<R>),
}

pub enum NodeEnumMut<'a, R> {
    File(&'a mut File<R>),
    Directory(&'a mut Directory<R>),
}

pub enum NodeEnum<R> {
    File(File<R>),
    Directory(Directory<R>),
}

pub struct GeckoFS<R> {
    pub(super) root: Directory<R>,
    pub(super) system: Directory<R>,
}

impl<R> GeckoFS<R>
where
    R: AsyncRead + AsyncSeek + 'static,
{
    pub fn new() -> Self {
        Self {
            root: Directory::new(""),
            system: Directory::new("&&systemdata"),
        }
    }

    #[doc = r"Utility function to read the disc."]
    async fn read_exact<R2: DerefMut<Target = DiscReader<R>>>(
        reader: &mut R2,
        pos: SeekFrom,
        buf: &mut [u8],
    ) -> Result<()> {
        reader.seek(pos).await?;
        Ok(reader.read_exact(buf).await?)
    }

    fn get_dir_structure_recursive(
        cur_index: &mut usize,
        fst: &Vec<FstEntry>,
        parent_dir: &mut Directory<R>,
        reader: &Arc<Mutex<DiscReader<R>>>,
    ) {
        let entry = &fst[*cur_index];

        match entry.kind {
            FstNodeType::Directory => {
                let dir = parent_dir.mkdir(entry.relative_file_name.clone());

                while *cur_index < entry.file_size_next_dir_index - 1 {
                    *cur_index += 1;
                    GeckoFS::get_dir_structure_recursive(cur_index, fst, dir, reader);
                }
            }
            FstNodeType::File => {
                parent_dir.add_file(File::new(
                    FileDataSource::Reader {
                        source: reader.clone(),
                        initial_offset: entry.file_offset_parent_dir.try_into().unwrap(),
                    },
                    entry.relative_file_name.clone(),
                    entry.file_offset_parent_dir,
                    entry.file_size_next_dir_index,
                    entry.file_name_offset,
                ));
            }
        }
    }

    pub async fn parse(reader: Arc<Mutex<DiscReader<R>>>) -> Result<Self> {
        let mut root = Directory::new("");
        let mut system = Directory::new("&&systemdata");
        {
            let mut guard = reader.lock_arc().await;
            let is_wii = guard.get_type() == DiscType::Wii;
            crate::debug!(
                "{}",
                if is_wii {
                    "The disc is a Wii game"
                } else {
                    "The disc is NOT a Wii game"
                }
            );
            let mut buf = [0u8; 4];
            GeckoFS::read_exact(
                &mut guard,
                SeekFrom::Start(consts::OFFSET_FST_OFFSET as u64),
                &mut buf,
            )
            .await?;
            let fst_offset = (BE::read_u32(&buf[..]) << (if is_wii { 2 } else { 0 })) as u64;
            GeckoFS::read_exact(&mut guard, SeekFrom::Start(fst_offset + 8), &mut buf).await?;
            let num_entries = BE::read_u32(&buf[..]) as usize;
            let mut fst_list_buf = vec![0u8; num_entries * FstEntry::BLOCK_SIZE];
            GeckoFS::read_exact(&mut guard, SeekFrom::Start(fst_offset), &mut fst_list_buf).await?;
            let string_table_offset = num_entries as u64 * FstEntry::BLOCK_SIZE as u64;

            GeckoFS::read_exact(
                &mut guard,
                SeekFrom::Start(consts::OFFSET_FST_SIZE as u64),
                &mut buf,
            )
            .await?;
            let fst_size = (BE::read_u32(&buf) as usize) << (if is_wii { 2 } else { 0 });
            let mut str_tbl_buf = vec![0u8; fst_size - string_table_offset as usize];
            GeckoFS::read_exact(
                &mut guard,
                SeekFrom::Start(string_table_offset + fst_offset),
                &mut str_tbl_buf,
            )
            .await?;

            crate::debug!(
                "#fst enties: {}; #names: {}",
                num_entries,
                str_tbl_buf.split(|b| *b == 0).count()
            );

            let fst_entries: Vec<FstEntry> = {
                let mut fst_entries: Vec<FstEntry> = Vec::new();
                for i in 0..num_entries {
                    let kind = FstNodeType::try_from(fst_list_buf[i * 12])
                        .unwrap_or(FstNodeType::Directory);

                    let string_offset =
                        (BE::read_u32(&fst_list_buf[i * 12..]) & 0x00ffffff) as usize;

                    let pos = string_offset;
                    let mut end = pos;
                    while str_tbl_buf[end] != 0 {
                        end += 1;
                    }
                    crate::trace!("entry #{} string size: {}", i, end - pos);
                    let mut str_buf = Vec::new();
                    str_buf.extend_from_slice(&str_tbl_buf[pos..end]);
                    let relative_file_name = String::from_utf8(str_buf)?;

                    let file_offset_parent_dir = (BE::read_u32(&fst_list_buf[i * 12 + 4..])
                        as usize)
                        << (if is_wii { 2 } else { 0 });
                    let file_size_next_dir_index =
                        BE::read_u32(&fst_list_buf[i * 12 + 8..]) as usize;

                    fst_entries.push(FstEntry {
                        kind,
                        relative_file_name,
                        file_offset_parent_dir,
                        file_size_next_dir_index,
                        file_name_offset: string_offset,
                    });
                    crate::trace!("parsed entry #{}: {:?}", i, fst_entries.last());
                }
                fst_entries
            };

            GeckoFS::read_exact(
                &mut guard,
                SeekFrom::Start(consts::OFFSET_DOL_OFFSET as u64),
                &mut buf,
            )
            .await?;
            let dol_offset = (BE::read_u32(&buf) as usize) << (if is_wii { 2 } else { 0 });
            crate::debug!(
                "fst_size: 0x{:08X}; fst entries list size: 0x{:08X}",
                fst_size,
                num_entries * FstEntry::BLOCK_SIZE
            );

            system.add_file(File::from_reader_at_position(
                reader.clone(),
                "iso.hdr",
                0,
                consts::HEADER_LENGTH,
                0,
            ));
            system.add_file(File::from_reader_at_position(
                reader.clone(),
                "AppLoader.ldr",
                consts::HEADER_LENGTH,
                dol_offset - consts::HEADER_LENGTH,
                0,
            ));
            system.add_file(File::from_reader_at_position(
                reader.clone(),
                "Start.dol",
                dol_offset,
                fst_offset as usize - dol_offset,
                0,
            ));
            system.add_file(File::from_reader_at_position(
                reader.clone(),
                "Game.toc",
                fst_offset as usize,
                fst_size,
                0,
            ));

            let mut count = 1;
            while count < num_entries {
                GeckoFS::get_dir_structure_recursive(&mut count, &fst_entries, &mut root, &reader);
                count += 1;
            }
        }
        crate::debug!("{} children", root.children.len());
        Ok(Self { root, system })
    }

    /// Calculates the length of the FST.
    fn fst_len(&self) -> u32 {
        self.root
            .iter()
            .map(|node| {
                u32::try_from(12 + node.name().len() + 1)
                    .expect("FST entry size exceeds 32-bit limit")
            })
            .reduce(|acc, e| acc.checked_add(e).expect("FST size exceeds 32-bit limit"))
            .unwrap_or(1)
            - 1
    }

    fn visitor_fst_entries(
        node: &mut dyn Node<R>,
        output_fst: &mut Vec<FstEntry>,
        files: &mut Vec<File<R>>,
        fst_name_bank: &mut Vec<u8>,
        cur_parent_dir_index: usize,
        offset: &mut u64,
    ) {
        match node.as_enum_mut() {
            NodeEnumMut::Directory(dir) => {
                let fst_entry = FstEntry {
                    kind: FstNodeType::Directory,
                    file_name_offset: fst_name_bank.len(),
                    file_offset_parent_dir: cur_parent_dir_index,
                    ..Default::default()
                };

                fst_name_bank.extend_from_slice(dir.name().as_bytes());
                fst_name_bank.push(0);

                let this_dir_index = output_fst.len();

                output_fst.push(fst_entry);

                for child in &mut dir.children {
                    GeckoFS::visitor_fst_entries(
                        child.as_mut(),
                        output_fst,
                        files,
                        fst_name_bank,
                        this_dir_index,
                        offset,
                    );
                }

                output_fst[this_dir_index].file_size_next_dir_index = output_fst.len();
            }
            NodeEnumMut::File(file) => {
                let pos = align_addr(*offset, 5);
                *offset = pos;

                let fst_entry = FstEntry {
                    kind: FstNodeType::File,
                    file_size_next_dir_index: file.len(),
                    file_name_offset: fst_name_bank.len(),
                    file_offset_parent_dir: pos as usize,
                    relative_file_name: file.name().to_string(),
                };

                fst_name_bank.extend_from_slice(file.name().as_bytes());
                fst_name_bank.push(0);

                *offset += file.len() as u64;
                *offset = align_addr(*offset, 2);

                file.fst = fst_entry.clone();
                output_fst.push(fst_entry);
                files.push(file.clone());
            }
        };
    }

    pub async fn serialize<W>(&mut self, writer: &mut W, is_wii: bool) -> Result<()>
    where
        W: AsyncWrite + AsyncSeek + Unpin,
    {
        crate::debug!("Serializing the FileSystem");
        let header_size = self.sys().get_file("iso.hdr")?.len();
        let apploader_size = self.sys().get_file("AppLoader.ldr")?.len();

        // Calculate dynamic offsets
        let dol_offset_raw = header_size + apploader_size;
        let dol_offset = align_addr(dol_offset_raw, consts::DOL_ALIGNMENT_BIT);
        let dol_padding_size = dol_offset - dol_offset_raw;
        let dol_size = self.sys().get_file("Start.dol")?.len();

        let fst_list_offset_raw = dol_offset + dol_size;
        let fst_list_offset = align_addr(fst_list_offset_raw, consts::FST_ALIGNMENT_BIT);
        let fst_list_padding_size = fst_list_offset - fst_list_offset_raw;

        let fst_len = self.fst_len();

        let d = [
            (dol_offset >> if is_wii { 2u8 } else { 0u8 }) as u32,
            (fst_list_offset >> if is_wii { 2u8 } else { 0u8 }) as u32,
            fst_len,
            fst_len,
        ];
        let mut b = vec![0u8; 0x10];
        BE::write_u32_into(&d, &mut b);

        // Write header and app loader
        let mut buf = Vec::new();
        self.sys_mut()
            .get_file_mut("iso.hdr")?
            .read_to_end(&mut buf)
            .await?;
        writer.write_all(&buf[..OFFSET_DOL_OFFSET]).await?;
        writer.write_all(&b).await?;
        writer.write_all(&buf[OFFSET_DOL_OFFSET + 0x10..]).await?;
        buf.clear();
        self.sys_mut()
            .get_file_mut("AppLoader.ldr")?
            .read_to_end(&mut buf)
            .await?;
        writer.write_all(&buf).await?;
        writer.write_all(&vec![0u8; dol_padding_size]).await?;

        buf.clear();
        self.sys_mut()
            .get_file_mut("Start.dol")?
            .read_to_end(&mut buf)
            .await?;
        writer.write_all(&buf).await?;
        writer.write_all(&vec![0u8; fst_list_padding_size]).await?;

        let mut output_fst = vec![FstEntry {
            kind: FstNodeType::Directory,
            file_size_next_dir_index: 0,
            ..Default::default()
        }];
        let mut fst_name_bank = Vec::new();
        let mut files = Vec::new();

        let mut offset = (fst_list_offset + fst_len as usize) as u64;
        for node in self.root_mut().children_mut() {
            GeckoFS::visitor_fst_entries(
                node.as_mut(),
                &mut output_fst,
                &mut files,
                &mut fst_name_bank,
                0,
                &mut offset,
            );
        }
        output_fst[0].file_size_next_dir_index = output_fst.len();
        crate::debug!("output_fst size = {}", output_fst.len());
        crate::debug!("first fst_name entry = {}", fst_name_bank[0]);
        #[cfg(feature = "progress")]
        let write_total_size: u64 = output_fst
            .iter()
            .filter_map(|f| {
                if f.kind == FstNodeType::File {
                    Some(f.file_size_next_dir_index as u64)
                } else {
                    None
                }
            })
            .sum();

        for entry in output_fst {
            let mut buf = [0u8; 12];
            BE::write_u32_into(
                &[
                    ((entry.kind as u32) << 24) | (entry.file_name_offset as u32 & 0x00FFFFFF),
                    (entry.file_offset_parent_dir >> if is_wii { 2u8 } else { 0u8 }) as u32,
                    entry.file_size_next_dir_index as u32,
                ],
                &mut buf,
            );
            writer.write_all(&buf).await?;
        }

        writer.write_all(&fst_name_bank).await?;

        // Traverse the root directory tree to write all the files in order
        #[cfg(feature = "progress")]
        if let Ok(mut updater) = UPDATER.lock() {
            updater.set_type(crate::update::UpdaterType::Progress)?;
            updater.init(Some(write_total_size as usize))?;
            updater.set_title("Writing virtual FileSystem".to_string())?;
        }
        let mut offset = writer.seek(SeekFrom::Current(0)).await? as usize;
        #[cfg(feature = "progress")]
        let mut inc_buffer = 0usize;
        for mut file in files {
            #[cfg(feature = "progress")]
            if let Ok(mut updater) = UPDATER.try_lock() {
                updater.set_message(format!(
                    "{:<32.32} ({:>8})",
                    file.name(),
                    human_bytes(file.len() as f64)
                ))?;
            }
            let padding_size = file.fst.file_offset_parent_dir - offset;
            writer.write_all(&vec![0u8; padding_size]).await?;
            // Copy the file from the FileSystem to the Writer.
            // async_std::io::copy(file, writer).await?; // way too slow
            let mut rem = file.len();
            loop {
                if rem == 0 {
                    break;
                }
                let transfer_size = std::cmp::min(rem, 1024 * 1024);
                let mut buf = vec![0u8; transfer_size];
                file.read_exact(&mut buf).await?;
                writer.write_all(&buf).await?;
                rem -= transfer_size;
                #[cfg(feature = "progress")]
                match UPDATER.try_lock() {
                    Ok(mut updater) => {
                        updater.increment(transfer_size + inc_buffer)?;
                        inc_buffer = 0;
                    }
                    Err(TryLockError::WouldBlock) => {
                        inc_buffer += transfer_size;
                    }
                    _ => (),
                }
            }
            offset += file.len() + padding_size;
        }
        #[cfg(feature = "progress")]
        if let Ok(mut updater) = UPDATER.lock() {
            updater.finish()?;
        }

        Ok(())
    }

    pub fn sys(&self) -> &Directory<R> {
        &self.system
    }

    pub fn sys_mut(&mut self) -> &mut Directory<R> {
        &mut self.system
    }

    pub fn root(&self) -> &Directory<R> {
        &self.root
    }

    pub fn root_mut(&mut self) -> &mut Directory<R> {
        &mut self.root
    }
}

impl<R> Default for GeckoFS<R>
where
    R: AsyncRead + AsyncSeek + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<R> Node<R> for GeckoFS<R>
where
    R: AsyncRead + AsyncSeek,
{
    fn name(&self) -> &str {
        self.root.name()
    }

    fn get_type(&self) -> NodeType {
        NodeType::Directory
    }

    fn into_directory(self) -> Option<Directory<R>> {
        Some(self.root)
    }

    fn as_directory_ref(&self) -> Option<&Directory<R>> {
        Some(&self.root)
    }

    fn as_directory_mut(&mut self) -> Option<&mut Directory<R>> {
        Some(&mut self.root)
    }

    fn into_file(self) -> Option<File<R>> {
        None
    }

    fn as_file_ref(&self) -> Option<&File<R>> {
        None
    }

    fn as_file_mut(&mut self) -> Option<&mut File<R>> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
enum FileReadState {
    #[default]
    Seeking,
    Reading,
}

#[derive(Debug, Clone, Copy, Default)]
struct FileState {
    cursor: u64,
    state: FileReadState,
}

#[derive(Debug)]
pub enum FileDataSource<R> {
    Reader {
        initial_offset: u64,
        source: Arc<Mutex<DiscReader<R>>>,
    },
    Box(Arc<Mutex<Box<[u8]>>>),
}

impl<R> Clone for FileDataSource<R> {
    fn clone(&self) -> Self {
        match self {
            Self::Reader {
                initial_offset,
                source,
            } => Self::Reader {
                initial_offset: *initial_offset,
                source: source.clone(),
            },
            Self::Box(arg0) => Self::Box(arg0.clone()),
        }
    }
}

#[derive(Debug)]
pub struct File<R> {
    fst: FstEntry,
    state: FileState,
    data: FileDataSource<R>,
}

impl<R> Clone for File<R> {
    fn clone(&self) -> Self {
        Self {
            fst: self.fst.clone(),
            state: self.state,
            data: self.data.clone(),
        }
    }
}

impl<R> File<R> {
    pub fn new<S: Into<String>>(
        data: FileDataSource<R>,
        name: S,
        file_offset_parent_dir: usize,
        file_size_next_dir_index: usize,
        file_name_offset: usize,
    ) -> Self {
        Self {
            fst: FstEntry {
                kind: FstNodeType::File,
                relative_file_name: name.into(),
                file_offset_parent_dir,
                file_size_next_dir_index,
                file_name_offset,
            },
            state: Default::default(),
            data,
        }
    }

    /// Creates a new File that reads itself from `source` at `offset` (i.e.
    /// identity mapping)
    pub fn from_reader_at_position<S: ToString>(
        source: Arc<Mutex<DiscReader<R>>>,
        name: S,
        offset: usize,
        length: usize,
        file_name_offset: usize,
    ) -> Self {
        Self {
            fst: FstEntry {
                kind: FstNodeType::File,
                relative_file_name: name.to_string(),
                file_offset_parent_dir: offset,
                file_size_next_dir_index: length,
                file_name_offset,
            },
            state: FileState::default(),
            data: FileDataSource::Reader {
                initial_offset: offset.try_into().unwrap(),
                source,
            },
        }
    }

    pub fn set_data(&mut self, data: Box<[u8]>) {
        self.fst.file_size_next_dir_index = data.len();
        self.data = FileDataSource::Box(Arc::new(Mutex::new(data)));
    }

    pub fn len(&self) -> usize {
        self.fst.file_size_next_dir_index
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn offset(&self) -> usize {
        self.fst.file_offset_parent_dir
    }
}

impl<R: AsyncSeek> AsyncSeek for File<R> {
    fn poll_seek(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<std::io::Result<u64>> {
        crate::trace!("Seeking \"{0}\" to {1:?} ({1:016X?})", self.name(), pos);
        let pos = match pos {
            SeekFrom::Start(pos) => {
                if pos > self.len() as u64 {
                    return Poll::Ready(Err(Error::new(
                        async_std::io::ErrorKind::Other,
                        eyre::eyre!("Index out of range"),
                    )));
                }
                SeekFrom::Start(pos)
            }
            SeekFrom::End(pos) => {
                let new_pos = self.len() as i64 + pos;
                if new_pos < 0 || pos > 0 {
                    return Poll::Ready(Err(Error::new(
                        async_std::io::ErrorKind::Other,
                        eyre::eyre!("Index out of range"),
                    )));
                }
                SeekFrom::End(pos)
            }
            SeekFrom::Current(pos) => {
                let new_pos = self.state.cursor as i64 + pos;
                if new_pos < 0 || new_pos > self.len() as i64 {
                    return Poll::Ready(Err(Error::new(
                        async_std::io::ErrorKind::Other,
                        eyre::eyre!("Index out of range"),
                    )));
                }
                SeekFrom::Current(pos)
            }
        };
        match &self.data {
            FileDataSource::Reader {
                source: reader,
                initial_offset,
            } => match reader.try_lock_arc() {
                Some(mut guard) => {
                    let guard_mut = guard.deref_mut();
                    let guard_pin = std::pin::pin!(guard_mut);
                    match guard_pin.poll_seek(
                        cx,
                        match pos {
                            SeekFrom::Start(pos) => SeekFrom::Start(initial_offset + pos),
                            SeekFrom::End(pos) => SeekFrom::Start(
                                ((*initial_offset as i64 + self.len() as i64) + pos) as u64,
                            ),
                            SeekFrom::Current(pos) => SeekFrom::Start(
                                (*initial_offset as i64 + self.state.cursor as i64 + pos) as u64,
                            ),
                        },
                    ) {
                        Poll::Ready(Ok(_)) => match pos {
                            SeekFrom::Start(pos) => {
                                self.state.cursor = pos;
                                Poll::Ready(Ok(self.state.cursor))
                            }
                            SeekFrom::End(pos) => {
                                self.state.cursor = (self.len() as i64 + pos) as u64;
                                Poll::Ready(Ok(self.state.cursor))
                            }
                            SeekFrom::Current(pos) => {
                                self.state.cursor = (self.state.cursor as i64 + pos) as u64;
                                Poll::Ready(Ok(self.state.cursor))
                            }
                        },
                        Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                        Poll::Pending => Poll::Pending,
                    }
                }
                None => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            },
            FileDataSource::Box(_) => match pos {
                SeekFrom::Start(pos) => {
                    self.state.cursor = pos;
                    Poll::Ready(Ok(self.state.cursor))
                }
                SeekFrom::End(pos) => {
                    self.state.cursor = (self.len() as i64 + pos) as u64;
                    Poll::Ready(Ok(self.state.cursor))
                }
                SeekFrom::Current(pos) => {
                    self.state.cursor = (self.state.cursor as i64 + pos) as u64;
                    Poll::Ready(Ok(self.state.cursor))
                }
            },
        }
    }
}

impl<R: AsyncRead + AsyncSeek> AsyncRead for File<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        crate::trace!(
            "Reading \"{}\" for 0x{:08X} byte(s)",
            self.name(),
            buf.len()
        );
        let end = std::cmp::min(
            buf.len(),
            (self.len() as i64 - self.state.cursor as i64) as usize,
        );
        match self.state.state {
            FileReadState::Seeking => match &self.data {
                FileDataSource::Reader {
                    source: reader,
                    initial_offset,
                } => match reader.try_lock_arc() {
                    Some(mut guard) => {
                        let guard_pin = std::pin::pin!(guard.deref_mut());
                        match guard_pin
                            .poll_seek(cx, SeekFrom::Start(initial_offset + self.state.cursor))
                        {
                            Poll::Ready(Ok(_)) => {
                                self.state.state = FileReadState::Reading;
                                cx.waker().wake_by_ref();
                                Poll::Pending
                            }
                            Poll::Ready(Err(err)) => {
                                self.state.state = FileReadState::Seeking;
                                Poll::Ready(Err(err))
                            }
                            Poll::Pending => Poll::Pending,
                        }
                    }
                    None => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                },
                FileDataSource::Box(_data) => {
                    if self.state.cursor > self.len() as u64 {
                        Poll::Ready(Err(io::Error::from(io::ErrorKind::InvalidInput)))
                    } else {
                        self.state.state = FileReadState::Reading;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
            },
            FileReadState::Reading => match &self.data {
                FileDataSource::Reader { source: reader, .. } => match reader.try_lock_arc() {
                    Some(mut guard) => {
                        let guard_pin = std::pin::pin!(guard.deref_mut());
                        match guard_pin.poll_read(cx, &mut buf[..end]) {
                            Poll::Ready(Ok(num_read)) => {
                                self.state.cursor += num_read as u64;
                                self.state.state = FileReadState::Seeking;
                                Poll::Ready(Ok(num_read))
                            }
                            Poll::Ready(Err(err)) => {
                                self.state.state = FileReadState::Seeking;
                                Poll::Ready(Err(err))
                            }
                            Poll::Pending => Poll::Pending,
                        }
                    }
                    None => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                },
                FileDataSource::Box(data) => {
                    let d: async_std::sync::MutexGuardArc<Box<[u8]>> = match data.try_lock_arc() {
                        Some(data) => data,
                        None => return Poll::Pending,
                    };
                    let num_read =
                        std::cmp::min(buf.len(), (self.len() as u64 - self.state.cursor) as usize);
                    buf[..num_read].copy_from_slice(&d[self.state.cursor as usize..][..num_read]);
                    self.state.cursor += num_read as u64;
                    self.state.state = FileReadState::Seeking;
                    Poll::Ready(Ok(num_read))
                }
            },
        }
    }
}

impl<R> Node<R> for File<R> {
    fn name(&self) -> &str {
        &self.fst.relative_file_name
    }

    fn get_type(&self) -> NodeType {
        NodeType::File
    }

    fn into_directory(self) -> Option<Directory<R>> {
        None
    }

    fn as_directory_ref(&self) -> Option<&Directory<R>> {
        None
    }

    fn as_directory_mut(&mut self) -> Option<&mut Directory<R>> {
        None
    }

    fn into_file(self) -> Option<File<R>> {
        Some(self)
    }

    fn as_file_ref(&self) -> Option<&File<R>> {
        Some(self)
    }

    fn as_file_mut(&mut self) -> Option<&mut File<R>> {
        Some(self)
    }
}

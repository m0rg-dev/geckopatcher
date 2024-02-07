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
                    FileDataSource::Reader(reader.clone()),
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

            // let root_name = (0, "".into());
            // let name_it = {
            //     let offsets = std::iter::once(0).chain(
            //         str_tbl_buf
            //             .iter()
            //             .enumerate()
            //             .filter_map(|(i, b)| if *b == 0 { Some(i + 1) } else { None })
            //             .take(num_entries - 1),
            //     );
            //     std::iter::once(root_name).chain(
            //         offsets.zip(
            //             str_tbl_buf
            //                 .split(|b| *b == 0)
            //                 .map(String::from_utf8_lossy)
            //                 .map(|s| s.to_string()),
            //         ),
            //     )
            // };

            #[cfg(disabled)]
            let fst_entries: Vec<FstEntry> = {
                fst_list_buf
                    .chunks_exact(FstEntry::BLOCK_SIZE)
                    .zip(name_it)
                    .enumerate()
            }
            .map(|(i, (entry_buf, (name_off, name)))| {
                let kind = FstNodeType::try_from(entry_buf[0]).unwrap_or(FstNodeType::Directory);

                let string_offset = (BE::read_u32(entry_buf) & 0x00ffffff) as usize;
                if string_offset != name_off {
                    crate::warn!(
                        "String offset for file \"{}\" differs (extracted {}, calculated: {}",
                        name,
                        string_offset,
                        name_off
                    );
                }

                // let pos = string_offset;
                // let mut end = pos;
                // while str_tbl_buf[end] != 0 {
                //     end += 1;
                // }
                // crate::trace!("entry #{} string size: {}", i, end - pos);
                // let mut str_buf = Vec::new();
                // str_buf.extend_from_slice(&str_tbl_buf[pos..end]);
                // let relative_file_name = String::from_utf8_lossy(&str_buf).to_string();
                let relative_file_name = name;

                let file_offset_parent_dir =
                    (BE::read_u32(&entry_buf[4..]) as usize) << (if is_wii { 2 } else { 0 });
                let file_size_next_dir_index = BE::read_u32(&entry_buf[8..]) as usize;

                let fst_entry = FstEntry {
                    kind,
                    relative_file_name,
                    file_offset_parent_dir,
                    file_size_next_dir_index,
                    file_name_offset: string_offset,
                };
                crate::trace!("parsed entry #{}: {:?}", i, fst_entry);
                fst_entry
            })
            .collect();
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

            system.add_file(File::new(
                FileDataSource::Reader(reader.clone()),
                "iso.hdr",
                0,
                consts::HEADER_LENGTH,
                0,
            ));
            system.add_file(File::new(
                FileDataSource::Reader(reader.clone()),
                "AppLoader.ldr",
                consts::HEADER_LENGTH,
                dol_offset - consts::HEADER_LENGTH,
                0,
            ));
            system.add_file(File::new(
                FileDataSource::Reader(reader.clone()),
                "Start.dol",
                dol_offset,
                fst_offset as usize - dol_offset,
                0,
            ));
            system.add_file(File::new(
                FileDataSource::Reader(reader.clone()),
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

    /// Visits the directory tree to calculate the length of the FST table
    fn visitor_fst_len(mut acc: usize, node: &dyn Node<R>) -> usize {
        match node.as_enum_ref() {
            NodeEnumRef::Directory(dir) => {
                acc += 12 + dir.name().len() + 1;

                for child in &dir.children {
                    acc = GeckoFS::visitor_fst_len(acc, child.as_ref());
                }
            }
            NodeEnumRef::File(file) => {
                acc += 12 + file.name().len() + 1;
            }
        };
        acc
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

                file.new_offset = pos;
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

        let fst_len = GeckoFS::visitor_fst_len(0, &self.root) - 1;

        let d = [
            (dol_offset >> if is_wii { 2u8 } else { 0u8 }) as u32,
            (fst_list_offset >> if is_wii { 2u8 } else { 0u8 }) as u32,
            fst_len as u32,
            fst_len as u32,
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

        let mut offset = (fst_list_offset + fst_len) as u64;
        for node in self.root_mut().iter_mut() {
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
            let padding_size = file.new_offset as usize - offset;
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

pub struct Directory<R> {
    name: String,
    children: Vec<Box<dyn Node<R>>>,
}

impl<R> Directory<R>
where
    R: 'static,
{
    pub fn new<S: Into<String>>(name: S) -> Directory<R> {
        Self {
            name: name.into(),
            children: Vec::new(),
        }
    }

    pub fn resolve_node(&self, path: &str) -> Option<&dyn Node<R>> {
        let mut dir = self;
        let mut segments = path.split('/').peekable();

        while let Some(segment) = segments.next() {
            if segments.peek().is_some() {
                // Must be a folder
                dir = dir
                    .children
                    .iter()
                    .filter_map(|c| c.as_directory_ref())
                    .find(|d| d.name == segment)?;
            } else {
                return dir
                    .children
                    .iter()
                    .filter_map(|c| c.as_file_ref())
                    .find(|f| f.name() == segment)
                    .map(|x| x as &dyn Node<R>);
            }
        }
        Some(dir)
    }

    pub fn resolve_node_mut(&mut self, path: &str) -> Option<&mut dyn Node<R>> {
        let mut dir = self;
        let mut segments = path.split('/').peekable();

        while let Some(segment) = segments.next() {
            if segments.peek().is_some() {
                // Must be a folder
                dir = dir
                    .children
                    .iter_mut()
                    .filter_map(|c| c.as_directory_mut())
                    .find(|d| d.name == segment)?;
            } else {
                return dir
                    .children
                    .iter_mut()
                    .filter_map(|c| c.as_file_mut())
                    .find(|f| f.name() == segment)
                    .map(|x| x as &mut dyn Node<R>);
            }
        }
        Some(dir)
    }

    pub fn mkdir(&mut self, name: String) -> &mut Directory<R> {
        if self.children.iter().all(|c| c.name() != name) {
            self.children.push(Box::new(Directory::new(name)));
            self.children
                .last_mut()
                .map(|x| x.as_directory_mut().unwrap())
                .unwrap()
        } else {
            self.children
                .iter_mut()
                .find(|c| c.name() == name)
                .map(|c| c.as_directory_mut().unwrap())
                .unwrap()
        }
    }

    pub fn add_file(&mut self, file: File<R>) -> &mut File<R> {
        if self.children.iter().all(|c| c.name() != file.name()) {
            self.children.push(Box::new(file));
            self.children
                .last_mut()
                .map(|c| c.as_file_mut().unwrap())
                .unwrap()
        } else {
            self.children
                .iter_mut()
                .find(|c| c.name() == file.name())
                .map(|c| c.as_file_mut().unwrap())
                .unwrap()
        }
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Box<dyn Node<R>>> {
        self.children.iter()
    }

    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, Box<dyn Node<R>>> {
        self.children.iter_mut()
    }

    pub fn iter_recurse(&self) -> impl Iterator<Item = &'_ File<R>> {
        crate::trace!("Start iter_recurse");
        fn traverse_depth<'b, R: 'static>(start: &'b dyn Node<R>, stack: &mut Vec<&'b File<R>>) {
            match start.as_enum_ref() {
                NodeEnumRef::File(file) => stack.push(file),
                NodeEnumRef::Directory(dir) => {
                    for child in &dir.children {
                        traverse_depth(child.as_ref(), stack);
                    }
                }
            }
        }
        let mut stack = Vec::new();
        traverse_depth(self, &mut stack);
        crate::debug!("{} fst files", stack.len());
        stack.into_iter()
    }

    pub fn iter_recurse_mut(&mut self) -> impl Iterator<Item = &'_ mut File<R>> {
        crate::trace!("Start iter_recurse_mut");
        fn traverse_depth<'b, R: 'static>(
            start: &'b mut dyn Node<R>,
            stack: &mut Vec<&'b mut File<R>>,
        ) {
            match start.as_enum_mut() {
                NodeEnumMut::File(file) => stack.push(file),
                NodeEnumMut::Directory(dir) => {
                    for child in &mut dir.children {
                        traverse_depth(child.as_mut(), stack);
                    }
                }
            }
        }
        let mut stack = Vec::new();
        traverse_depth(self, &mut stack);
        crate::debug!("{} fst files", stack.len());
        stack.into_iter()
    }

    pub fn get_file(&self, path: &str) -> Result<&File<R>> {
        let self_name = self.name().to_owned();
        self.resolve_node(path)
            .ok_or(eyre::eyre!(
                "\"{path}\" not found in the directory \"{self_name}\""
            ))?
            .as_file_ref()
            .ok_or(eyre::eyre!("\"{path}\" is not a File!"))
    }

    pub fn get_file_mut(&mut self, path: &str) -> Result<&mut File<R>> {
        let self_name = self.name().to_owned();
        self.resolve_node_mut(path)
            .ok_or(eyre::eyre!(
                "\"{path}\" not found in the directory \"{self_name}\""
            ))?
            .as_file_mut()
            .ok_or(eyre::eyre!("\"{path}\" is not a File!"))
    }

    pub fn get_dir(&self, path: &str) -> Result<&Directory<R>> {
        let self_name = self.name().to_owned();
        self.resolve_node(path)
            .ok_or(eyre::eyre!(
                "\"{path}\" not found in the directory \"{self_name}\""
            ))?
            .as_directory_ref()
            .ok_or(eyre::eyre!("\"{path}\" is not a Directory!"))
    }

    pub fn get_dir_mut(&mut self, path: &str) -> Result<&mut Directory<R>> {
        let self_name = self.name().to_owned();
        self.resolve_node_mut(path)
            .ok_or(eyre::eyre!(
                "\"{path}\" not found in the directory \"{self_name}\""
            ))?
            .as_directory_mut()
            .ok_or(eyre::eyre!("\"{path}\" is not a Directory!"))
    }
}

impl<R> Node<R> for Directory<R> {
    fn name(&self) -> &str {
        &self.name
    }

    fn get_type(&self) -> NodeType {
        NodeType::Directory
    }

    fn into_directory(self) -> Option<Directory<R>> {
        Some(self)
    }

    fn as_directory_ref(&self) -> Option<&Directory<R>> {
        Some(self)
    }

    fn as_directory_mut(&mut self) -> Option<&mut Directory<R>> {
        Some(self)
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
    Reader(Arc<Mutex<DiscReader<R>>>),
    Box(Arc<Mutex<Box<[u8]>>>),
}

impl<R> Clone for FileDataSource<R> {
    fn clone(&self) -> Self {
        match self {
            Self::Reader(arg0) => Self::Reader(arg0.clone()),
            Self::Box(arg0) => Self::Box(arg0.clone()),
        }
    }
}

#[derive(Debug)]
pub struct File<R> {
    fst: FstEntry,
    new_offset: u64,
    state: FileState,
    data: FileDataSource<R>,
}

impl<R> Clone for File<R> {
    fn clone(&self) -> Self {
        Self {
            fst: self.fst.clone(),
            new_offset: self.new_offset,
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
            new_offset: 0,
            state: Default::default(),
            data,
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
            FileDataSource::Reader(reader) => match reader.try_lock_arc() {
                Some(mut guard) => {
                    let guard_mut = guard.deref_mut();
                    let guard_pin = std::pin::pin!(guard_mut);
                    match guard_pin.poll_seek(
                        cx,
                        match pos {
                            SeekFrom::Start(pos) => {
                                SeekFrom::Start(self.fst.file_offset_parent_dir as u64 + pos)
                            }
                            SeekFrom::End(pos) => SeekFrom::Start(
                                ((self.fst.file_offset_parent_dir as i64 + self.len() as i64) + pos)
                                    as u64,
                            ),
                            SeekFrom::Current(pos) => SeekFrom::Start(
                                (self.fst.file_offset_parent_dir as i64
                                    + self.state.cursor as i64
                                    + pos) as u64,
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
                FileDataSource::Reader(reader) => match reader.try_lock_arc() {
                    Some(mut guard) => {
                        let guard_pin = std::pin::pin!(guard.deref_mut());
                        match guard_pin.poll_seek(
                            cx,
                            SeekFrom::Start(
                                self.fst.file_offset_parent_dir as u64 + self.state.cursor,
                            ),
                        ) {
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
                FileDataSource::Reader(reader) => match reader.try_lock_arc() {
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

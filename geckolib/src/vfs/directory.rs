use super::{File, Node, NodeEnumMut, NodeEnumRef, NodeType};
use eyre::Result;

pub struct Directory<R> {
    name: String,
    pub(crate) children: Vec<Box<dyn Node<R>>>,
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

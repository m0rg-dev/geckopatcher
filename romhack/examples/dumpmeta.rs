use std::{path::PathBuf, sync::Arc};

use async_std::{
    io::{BufReader, ReadExt},
    sync::Mutex,
};
use clap::{Parser, ValueHint};
use geckolib::{
    iso::read::DiscReader,
    vfs::{Directory, GeckoFS, Node},
};

#[derive(Debug, Parser)]
#[command(author, version)]
struct Args {
    #[arg(value_hint = ValueHint::FilePath)]
    /// Game file to reprocess
    source: PathBuf,
}

fn dump_tree<R: 'static>(dir: &Directory<R>, depth: usize) {
    println!("{}{}", "  ".repeat(depth), dir.name());
    for entry in dir.children() {
        if let Some(subdir) = entry.as_directory_ref() {
            dump_tree(subdir, depth + 1);
        }
    }
}

fn main() -> color_eyre::eyre::Result<()> {
    color_eyre::install()?;
    #[cfg(feature = "log")]
    env_logger::init();

    let args = Args::parse();
    async_std::task::block_on(async {
        let file_reader = BufReader::with_capacity(
            0x7C00 * 64 * 8,
            async_std::fs::File::open(args.source).await?,
        );

        let mut fs =
            GeckoFS::parse(Arc::new(Mutex::new(DiscReader::new(file_reader).await?))).await?;

        println!("root tree:");
        dump_tree(fs.root(), 0);

        println!("root contents:");
        for f in fs.root_mut().iter_recurse_mut() {
            let mut buf = [0u8; 16];
            f.read_exact(&mut buf[..]).await?;

            println!(
                "{:>32} off=0x{:0>8x} len={:<10} incipit={buf:x?}",
                f.name(),
                f.offset(),
                f.len()
            );
        }

        println!("sys contents:");
        for f in fs.sys().iter_recurse() {
            println!(
                "{:>32} off=0x{:0>8x} len={:<10} ",
                f.name(),
                f.offset(),
                f.len()
            );
        }

        Ok::<_, color_eyre::eyre::Error>(())
    })?;

    Ok(())
}

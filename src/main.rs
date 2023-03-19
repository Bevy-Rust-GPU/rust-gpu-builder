use std::{
    error::Error,
    path::{Path, PathBuf},
};

use clap::{error::ErrorKind, Parser};

use futures::{
    channel::mpsc::{self, channel, Receiver, UnboundedSender},
    executor::{self, ThreadPool},
    select, SinkExt, StreamExt,
};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

use spirv_builder::{
    CompileResult, MetadataPrintout, SpirvBuilder, SpirvBuilderError, SpirvMetadata,
};

use tracing::{error, info};

/// Instantiate an async watcher and return it alongside a channel to receive events on.
fn async_watcher() -> notify::Result<(RecommendedWatcher, Receiver<notify::Result<Event>>)> {
    let (mut tx, rx) = channel(1);

    // Automatically select the best implementation for your platform.
    // You can also access each implementation directly e.g. INotifyWatcher.
    let watcher = RecommendedWatcher::new(
        move |res| {
            futures::executor::block_on(async {
                tx.send(res).await.unwrap();
            })
        },
        Default::default(),
    )?;

    Ok((watcher, rx))
}

/// Watch a file or directory, sending relevant events through the provided channel.
async fn async_watch<P: AsRef<Path>>(
    path: P,
    mut change_tx: UnboundedSender<()>,
) -> Result<(), Box<dyn Error>> {
    let path = path.as_ref();
    let path = std::fs::canonicalize(path)
        .unwrap_or_else(|e| panic!("Failed to canonicalize path {path:?}: {e:}"));

    let (mut watcher, mut rx) = async_watcher()?;

    // Add a path to be watched. All files and directories at that path and
    // below will be monitored for changes.
    let watch_path = if path.is_dir() {
        path.clone()
    } else {
        path.parent().unwrap().to_owned()
    };
    watcher.watch(watch_path.as_ref(), RecursiveMode::Recursive)?;

    while let Some(res) = rx.next().await {
        match res {
            Ok(event) => {
                if path.is_dir()
                    || event
                        .paths
                        .iter()
                        .find(|candidate| **candidate == path)
                        .is_some()
                {
                    change_tx.send(()).await.unwrap();
                }
            }
            Err(e) => error!("Watch error: {:?}", e),
        }
    }

    Ok(())
}

/// Clap value parser for `SpirvMetadata`.
fn spirv_metadata(s: &str) -> Result<SpirvMetadata, clap::Error> {
    match s {
        "none" => Ok(SpirvMetadata::None),
        "name-variables" => Ok(SpirvMetadata::NameVariables),
        "full" => Ok(SpirvMetadata::Full),
        _ => Err(clap::Error::new(ErrorKind::InvalidValue)),
    }
}

/// Clap application struct.
#[derive(Debug, Clone, Parser)]
#[command(author, version, about, long_about = None)]
struct ShaderBuilder {
    /// Shader crate to compile.
    path_to_crate: PathBuf,
    /// rust-gpu compile target.
    #[arg(short, long, default_value = "spirv-unknown-vulkan1.2")]
    target: String,
    /// Treat warnings as errors during compilation.
    #[arg(long, default_value = "false")]
    deny_warnings: bool,
    /// Compile shaders in release mode.
    #[arg(long, default_value = "true")]
    release: bool,
    /// Compile one .spv file per entry point.
    #[arg(long, default_value = "false")]
    multimodule: bool,
    /// Set the level of metadata included in the SPIR-V binary.
    #[arg(long, value_parser=spirv_metadata, default_value = "none")]
    spirv_metadata: SpirvMetadata,
    /// Allow store from one struct type to a different type with compatible layout and members.
    #[arg(long, default_value = "false")]
    relax_struct_store: bool,
    /// Allow allocating an object of a pointer type and returning a pointer value from a function
    /// in logical addressing mode.
    #[arg(long, default_value = "false")]
    relax_logical_pointer: bool,
    /// Enable VK_KHR_relaxed_block_layout when checking standard uniform,
    /// storage buffer, and push constant layouts.
    /// This is the default when targeting Vulkan 1.1 or later.
    #[arg(long, default_value = "false")]
    relax_block_layout: bool,
    /// Enable VK_KHR_uniform_buffer_standard_layout when checking standard uniform buffer layouts.
    #[arg(long, default_value = "false")]
    uniform_buffer_standard_layout: bool,
    /// Enable VK_EXT_scalar_block_layout when checking standard uniform, storage buffer, and push
    /// constant layouts.
    /// Scalar layout rules are more permissive than relaxed block layout so in effect this will
    /// override the --relax-block-layout option.
    #[arg(long, default_value = "false")]
    scalar_block_layout: bool,
    /// Skip checking standard uniform / storage buffer layout. Overrides any --relax-block-layout
    /// or --scalar-block-layout option.
    #[arg(long, default_value = "false")]
    skip_block_layout: bool,
    /// Preserve unused descriptor bindings. Useful for reflection.
    #[arg(long, default_value = "false")]
    preserve_bindings: bool,
    /// If set, will watch the provided directory and recompile on change.
    ///
    /// Can be specified multiple times to watch more than one directory.
    #[arg(short, long)]
    watch_paths: Option<Vec<String>>,
}

impl ShaderBuilder {
    /// Builds a shader with the provided set of options.
    pub fn build_shader(&self) -> Result<CompileResult, SpirvBuilderError> {
        SpirvBuilder::new(&self.path_to_crate, &self.target)
            .deny_warnings(self.deny_warnings)
            .release(self.release)
            .multimodule(self.multimodule)
            .spirv_metadata(self.spirv_metadata)
            .relax_struct_store(self.relax_struct_store)
            .relax_logical_pointer(self.relax_logical_pointer)
            .relax_block_layout(self.relax_block_layout)
            .uniform_buffer_standard_layout(self.uniform_buffer_standard_layout)
            .scalar_block_layout(self.scalar_block_layout)
            .skip_block_layout(self.skip_block_layout)
            .preserve_bindings(self.preserve_bindings)
            .print_metadata(MetadataPrintout::None)
            .build()
    }
}

fn main() {
    tracing_subscriber::fmt().init();

    let args = ShaderBuilder::parse();

    println!();
    info!("Shader Builder");
    println!();

    info!("Building shader...");
    if args.build_shader().is_ok() {
        info!("Build complete!");
    } else {
        error!("Build failed!");
    }
    println!();

    if args.watch_paths.is_none() {
        return;
    };

    let pool = ThreadPool::new().expect("Failed to build pool");
    let (change_tx, mut change_rx) = mpsc::unbounded::<()>();
    let (build_tx, mut build_rx) = mpsc::unbounded::<bool>();

    let mut building = false;

    let fut_values = async move {
        let mut args = args;

        let Some(watch_paths) = args.watch_paths.take() else {
            unreachable!();
        };

        println!();
        {
            for path in watch_paths {
                info!("Watching {path:} for changes...");
                let change_tx = change_tx.clone();
                pool.spawn_ok(async move {
                    async_watch(path, change_tx).await.unwrap();
                });
            }
        }

        loop {
            let mut file_change = change_rx.next();
            let mut build_complete = build_rx.next();
            select! {
                _ = file_change => {
                    if !building {
                        building = true;
                        info!("Building shader...");
                        pool.spawn_ok({
                            let mut build_tx = build_tx.clone();
                            let args = args.clone();
                            async move {
                                build_tx.send(args.build_shader().is_ok()).await.unwrap();
                            }
                        })
                    }
                },
                result = build_complete => {
                    let result = result.unwrap();
                    if result {
                        info!("Build complete!");
                    }
                    else {
                        error!("Build failed!");
                    }
                    println!();
                    building = false;
                }
            };
        }
    };

    executor::block_on(fut_values);
}

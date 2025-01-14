use std::{io::Write, path::PathBuf};

use clap::{CommandFactory, Parser, Subcommand};
use clipcat_base::{ClipEntryMetadata, ClipboardKind};
use clipcat_client::{Client, Manager, System};
use clipcat_external_editor::ExternalEditor;
use snafu::ResultExt;
use tokio::runtime::Runtime;

use crate::{
    config::Config,
    error::{self, Error},
    finder::{FinderRunner, FinderType},
};

const PREVIEW_LENGTH: usize = 80;

#[derive(Debug, Parser)]
#[clap(name = clipcat_base::MENU_PROGRAM_NAME, author, version, about, long_about = None)]
pub struct Cli {
    #[clap(subcommand)]
    commands: Option<Commands>,

    #[clap(
        long = "config",
        short = 'c',
        env = "CLIPCAT_MENU_CONFIG_FILE_PATH",
        help = "Specify a configuration file"
    )]
    config_file: Option<PathBuf>,

    #[clap(long, short = 'f', env = "CLIPCAT_MENU_FINDER", help = "Specify a finder")]
    finder: Option<FinderType>,

    #[clap(
        long,
        short = 'm',
        env = "CLIPCAT_MENU_MENU_LENGTH",
        help = "Specify the menu length of finder"
    )]
    menu_length: Option<usize>,

    #[clap(
        long,
        short = 'l',
        env = "CLIPCAT_MENU_LINE_LENGTH",
        help = "Specify the length of a line showing on finder"
    )]
    line_length: Option<usize>,

    #[clap(long = "log-level", env = "CLIPCAT_MENU_LOG_LEVEL", help = "Specify a log level")]
    log_level: Option<tracing::Level>,
}

#[allow(variant_size_differences)]
#[derive(Debug, Subcommand)]
pub enum Commands {
    #[clap(about = "Print the client and server version information")]
    Version {
        #[clap(long = "client", help = "If true, shows client version only (no server required).")]
        client: bool,
    },

    #[clap(about = "Output shell completion code for the specified shell (bash, zsh, fish)")]
    Completions { shell: clap_complete::Shell },

    #[clap(about = "Output default configuration")]
    DefaultConfig,

    #[clap(about = "Print available text finders")]
    ListFinder,

    #[clap(about = "Insert selected clip into clipboard")]
    Insert {
        #[clap(
            long = "kinds",
            short = 'k',
            default_value = "clipboard",
            help = "Specify which clipboard to insert (\"clipboard\", \"primary\", \"secondary\")"
        )]
        kinds: Vec<ClipboardKind>,
    },

    #[clap(
        aliases = &["rm", "delete", "del"],
        about = "Remove selected clip")]
    Remove,

    #[clap(about = "Edit selected clip")]
    Edit {
        #[clap(long = "editor", short = 'e', env = "EDITOR", help = "Specify a external editor")]
        editor: String,
    },
}

impl Default for Cli {
    fn default() -> Self { Self::parse() }
}

impl Cli {
    pub fn run(self) -> Result<(), Error> {
        let Self { commands, config_file, finder, menu_length, line_length, log_level } = self;

        match commands {
            Some(Commands::Version { client }) if client => {
                print_only_client_version();
                return Ok(());
            }
            Some(Commands::Completions { shell }) => {
                let mut app = Self::command();
                let bin_name = app.get_name().to_string();
                clap_complete::generate(shell, &mut app, bin_name, &mut std::io::stdout());

                return Ok(());
            }
            Some(Commands::DefaultConfig) => {
                let config_text =
                    toml::to_string_pretty(&Config::default()).expect("Config is serializable");
                std::io::stdout()
                    .write_all(config_text.as_bytes())
                    .expect("Failed to write to stdout");
                return Ok(());
            }
            Some(Commands::ListFinder) => {
                for ty in FinderType::available_types() {
                    println!("{ty}");
                }
                return Ok(());
            }
            _ => {}
        }

        let mut config = Config::load_or_default(config_file.unwrap_or_else(Config::default_path));
        if let Some(log_level) = log_level {
            config.log.level = log_level;
        }

        config.log.registry();

        let finder = {
            if let Some(finder) = finder {
                config.finder = finder;
            }

            let mut finder = FinderRunner::from_config(&config);
            if let Some(line_length) = line_length {
                finder.set_line_length(line_length);
            }

            if let Some(menu_length) = menu_length {
                finder.set_menu_length(menu_length);
            }
            finder
        };

        let fut = async move {
            let client = Client::new(config.server_endpoint).await?;
            let clips = client.list(PREVIEW_LENGTH).await?;

            match commands {
                Some(Commands::Version { .. }) => print_version(&client).await,
                Some(Commands::Insert { mut kinds }) => {
                    if kinds.is_empty() {
                        kinds.push(ClipboardKind::Clipboard);
                    } else {
                        kinds.sort_unstable();
                        kinds.dedup();
                    }

                    insert_clip(&clips, &finder, &client, &kinds).await?;
                }
                None => insert_clip(&clips, &finder, &client, &[ClipboardKind::Clipboard]).await?,
                Some(Commands::Remove) => {
                    let selections = finder.multiple_select(&clips).await?;
                    let ids: Vec<_> = selections.into_iter().map(|(_, clip)| clip.id).collect();
                    let removed_ids = client.batch_remove(&ids).await?;
                    for id in removed_ids {
                        tracing::info!("Removing clip (id: {:016x})", id);
                    }
                }
                Some(Commands::Edit { editor }) => {
                    let selection = finder.single_select(&clips).await?;
                    if let Some((_index, metadata)) = selection {
                        let clip = client.get(metadata.id).await?;
                        if clip.is_utf8_string() {
                            let editor = ExternalEditor::new(editor);
                            let new_data = editor
                                .execute(&clip.as_utf8_string())
                                .await
                                .context(error::CallEditorSnafu)?;
                            let (ok, new_id) =
                                client.update(clip.id(), new_data.as_bytes(), clip.mime()).await?;
                            if ok {
                                tracing::info!("Editing clip (id: {:016x})", new_id);
                            }
                            let _ok = client.mark(new_id, ClipboardKind::Clipboard).await?;
                            drop(client);
                        }
                    } else {
                        tracing::info!("Nothing is selected");
                        return Ok(());
                    }
                }
                _ => unreachable!(),
            }

            Ok(())
        };

        Runtime::new().context(error::InitializeTokioRuntimeSnafu)?.block_on(fut)
    }
}

async fn insert_clip(
    clips: &[ClipEntryMetadata],
    finder: &FinderRunner,
    client: &Client,
    clipboard_kinds: &[ClipboardKind],
) -> Result<(), Error> {
    let selection = finder.single_select(clips).await?;
    if let Some((index, clip)) = selection {
        tracing::info!("Inserting clip (index: {index}, id: {:016x})", clip.id);
        for &clipboard_kind in clipboard_kinds {
            let _ok = client.mark(clip.id, clipboard_kind).await?;
        }
    } else {
        tracing::info!("Nothing is selected");
    }

    Ok(())
}

fn print_only_client_version() {
    let client_version = Cli::command().get_version().unwrap_or_default().to_string();
    std::io::stdout()
        .write_all(Cli::command().render_long_version().as_bytes())
        .expect("Failed to write to stdout");
    std::io::stdout()
        .write_all(format!("Client Version: {client_version}\n").as_bytes())
        .expect("Failed to write to stdout");
}

async fn print_version(client: &Client) {
    let client_version = Cli::command().get_version().unwrap_or_default().to_string();
    let server_version = client
        .get_version()
        .await
        .map_or_else(|_err| "unknown".to_string(), |version| version.to_string());

    let info = format!("Client Version: {client_version}\nServer Version: {server_version}\n",);
    std::io::stdout()
        .write_all(Cli::command().render_long_version().as_bytes())
        .expect("Failed to write to stdout");
    std::io::stdout().write_all(info.as_bytes()).expect("Failed to write to stdout");
}

use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use clap::ValueEnum;

use crate::adapters::fs::expand_tilde;
use crate::adapters::sqlite::pool::open_db;
use crate::adapters::sqlite::schema::apply_schema;
use crate::app::config::{self, Config, FusionKind};
use crate::domains::embeddings::Embedder;
use crate::domains::ground::{ground, GroundOpts, GroundResponse};
use crate::domains::search::Fusion;

const DEFAULT_LIMIT: usize = 50;

#[derive(Debug, Default, Clone)]
pub struct GroundArgs {
    pub query: String,
    pub pretty: bool,
    pub top_files: Option<usize>,
    pub chunks_per_file: Option<usize>,
    pub fusion: Option<FusionChoice>,
    pub limit: Option<usize>,
    pub config: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FusionChoice {
    Rrf,
    Convex,
}

impl From<FusionChoice> for FusionKind {
    fn from(c: FusionChoice) -> Self {
        match c {
            FusionChoice::Rrf => FusionKind::Rrf,
            FusionChoice::Convex => FusionKind::Convex,
        }
    }
}

pub fn cmd_ground(args: GroundArgs) -> anyhow::Result<()> {
    let pretty = args.pretty;
    let response = run_ground(args)?;
    let text = if pretty {
        serde_json::to_string_pretty(&response)?
    } else {
        serde_json::to_string(&response)?
    };
    println!("{text}");
    Ok(())
}

pub fn run_ground(args: GroundArgs) -> anyhow::Result<GroundResponse> {
    let cfg = config::load(args.config.as_deref())?;
    let opts = ground_opts(&cfg, &args);
    let conn = open_database(&cfg)?;
    apply_schema(&conn)?;
    let cache_dir = expand_tilde(&cfg.embeddings.cache_dir);
    let mut embedder = Embedder::try_new(&cfg.embeddings.model, &cache_dir, &conn)
        .with_context(|| format!("init embedder ({})", cfg.embeddings.model))?;
    Ok(ground(&args.query, &conn, &mut embedder, opts)?)
}

fn ground_opts(cfg: &Config, args: &GroundArgs) -> GroundOpts {
    let kind: FusionKind = args
        .fusion
        .map(FusionKind::from)
        .unwrap_or(cfg.search.fusion);
    let fusion = match kind {
        FusionKind::Rrf => Fusion::Rrf { k: cfg.search.rrf_k },
        FusionKind::Convex => Fusion::Convex {
            alpha: cfg.search.convex_alpha,
        },
    };
    GroundOpts {
        top_files: args.top_files.unwrap_or(cfg.search.top_files_default),
        chunks_per_file: args
            .chunks_per_file
            .unwrap_or(cfg.search.chunks_per_file_default),
        fusion,
        limit: args.limit.unwrap_or(DEFAULT_LIMIT),
    }
}

fn open_database(cfg: &Config) -> anyhow::Result<rusqlite::Connection> {
    let db_path = expand_tilde(&cfg.storage.db_path);
    if let Some(parent) = db_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("create db parent {}", parent.display()))?;
    }
    open_db(&db_path).with_context(|| format!("open db at {}", db_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ground_opts_uses_config_defaults_when_args_unset() {
        let cfg = Config::default();
        let opts = ground_opts(&cfg, &GroundArgs::default());
        assert_eq!(opts.top_files, cfg.search.top_files_default);
        assert_eq!(opts.chunks_per_file, cfg.search.chunks_per_file_default);
        assert_eq!(opts.limit, DEFAULT_LIMIT);
        match opts.fusion {
            Fusion::Rrf { k } => assert_eq!(k, cfg.search.rrf_k),
            other => panic!("expected RRF default, got {other:?}"),
        }
    }

    #[test]
    fn ground_opts_overrides_with_args() {
        let cfg = Config::default();
        let args = GroundArgs {
            top_files: Some(2),
            chunks_per_file: Some(1),
            fusion: Some(FusionChoice::Convex),
            limit: Some(7),
            ..Default::default()
        };
        let opts = ground_opts(&cfg, &args);
        assert_eq!(opts.top_files, 2);
        assert_eq!(opts.chunks_per_file, 1);
        assert_eq!(opts.limit, 7);
        match opts.fusion {
            Fusion::Convex { alpha } => {
                assert!((alpha - cfg.search.convex_alpha).abs() < 1e-12);
            }
            other => panic!("expected convex, got {other:?}"),
        }
    }
}

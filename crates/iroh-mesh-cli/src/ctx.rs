//! The `imesh ctx` commands: contexts and their connections in the CLI config.

use anyhow::{Result, ensure};

use crate::{
    cli::{At, CtxCmd},
    config::{Config, Connection},
};

pub(crate) fn ctx(at: &At, cmd: CtxCmd) -> Result<()> {
    let cfg = Config::load()?;
    match cmd {
        CtxCmd::Ls => {
            println!("{:<16} {:<8} CONNECTIONS", "NAME", "CURRENT");
            for (name, c) in &cfg.contexts {
                let cur = if cfg.current_context.as_deref() == Some(name) { "*" } else { "" };
                println!("{name:<16} {cur:<8} {}", c.connections.len());
            }
        }
        CtxCmd::Use { name } => {
            Config::edit(|cfg| {
                ensure!(cfg.contexts.contains_key(&name), "context {name:?} not found");
                cfg.current_context = Some(name.clone());
                Ok(())
            })?;
            println!("current context is now {name:?}");
        }
        CtxCmd::Conn { index } => {
            let connections = if let Some(i) = index {
                Config::edit(|cfg| {
                    let ctx = cfg.context_mut(at.context.as_deref())?;
                    ensure!(i < ctx.connections.len(), "no connection {i}");
                    let c = ctx.connections.remove(i);
                    ctx.connections.insert(0, c);
                    Ok(ctx.connections.clone())
                })?
            } else {
                cfg.context(at.context.as_deref())?.1.connections.clone()
            };
            for (i, c) in connections.iter().enumerate() {
                println!("{i}  {c}");
            }
        }
        CtxCmd::Add { name, connections } => {
            let parsed = connections.iter().map(|c| Connection::parse(c)).collect::<Result<Vec<_>>>()?;
            let n = Config::edit(|cfg| {
                let ctx = cfg.contexts.entry(name.clone()).or_default();
                ctx.connections.extend(parsed);
                let n = ctx.connections.len();
                cfg.current_context.get_or_insert_with(|| name.clone());
                Ok(n)
            })?;
            println!("context {name:?} has {n} connections");
        }
    }
    Ok(())
}

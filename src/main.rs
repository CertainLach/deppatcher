use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    path::PathBuf,
    process::Command,
};

use cargo_lock::{Lockfile, Package};
use jrsonnet_evaluator::{
    error::{Error, Result},
    typed::Either2,
    typed::{Null, Typed},
    Either, State,
};
use toml_edit::{Document, InlineTable, Item, Table, Value};
use tracing::info;

#[derive(Typed, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct PackageDescription {
    name: String,
    source: String,
}

#[derive(Typed, Debug)]
struct SourceDescription {
    path: Option<String>,
    git: Option<String>,
    branch: Option<String>,

    #[typed(rename = "goDeep")]
    go_deep: Option<bool>,
}

fn find_packages_by_source(
    s: State,
    lock: &Lockfile,
    pkg: &Package,
    out: &mut BTreeMap<PackageDescription, SourceDescription>,
    visited: &mut BTreeSet<PackageDescription>,
    mapper: &impl Fn(State, PackageDescription) -> Result<Either![Null, SourceDescription]>,
) -> Result<()> {
    let desc = PackageDescription {
        name: pkg.name.to_string(),
        source: pkg.source.as_ref().map_or_else(
            || "".to_owned(),
            |s| {
                if s.is_default_registry() {
                    "crates-io".to_string()
                } else {
                    s.display_registry_name()
                }
            },
        ),
    };
    if visited.contains(&desc) {
        return Ok(());
    }
    if let Either2::B(source) = mapper(s.clone(), desc.clone())? {
        let go_deep = source.go_deep.unwrap_or(false);
        out.insert(desc.clone(), source);
        if !go_deep {
            visited.insert(desc);
            return Ok(());
        }
    }
    visited.insert(desc);

    for dependency in &pkg.dependencies {
        for pkg in lock.packages.iter().filter(|pkg| dependency.matches(pkg)) {
            find_packages_by_source(s.clone(), lock, pkg, out, visited, mapper)?;
        }
    }

    Ok(())
}

fn to_runtime(err: impl ToString) -> Error {
    Error::RuntimeError(err.to_string().into())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // {
    //     info!("removing patch section");
    //     let toml = fs::read_to_string("./Cargo.toml").map_err(to_runtime)?;

    //     let mut document = toml.parse::<Document>().map_err(to_runtime)?;
    //     let table = document.as_table_mut();

    //     table.remove("patch");

    //     fs::write("./Cargo.toml", document.to_string()).map_err(to_runtime)?;
    // }

    let mut checked_lockfiles: HashSet<String> = HashSet::new();

    let mut patches = BTreeMap::new();
    loop {
        {
            info!("regenerating lockfile");
            let _lock = Command::new("cargo")
                .arg("generate-lockfile")
                .status()
                .map_err(to_runtime)?;
        }
        let lock = Lockfile::load("./Cargo.lock").map_err(to_runtime)?;
        if checked_lockfiles.contains(&lock.to_string()) {
            info!("lockfile remaint same");
            break;
        }

        {
            info!("unique sources:");
            let mut out = BTreeSet::new();
            for p in &lock.packages {
                out.insert(
                    p.source
                        .as_ref()
                        .cloned()
                        .unwrap_or_default()
                        .display_registry_name(),
                );
            }
            for v in out {
                info!("- {}", v);
            }
        }

        info!("generating new patches");
        let s = State::default();
        s.set_import_resolver(Box::new(jrsonnet_evaluator::FileImportResolver::default()));
        let f = s.evaluate_file_raw(&PathBuf::from("./Cargo.dpp"))?;

        let f = f.as_func().unwrap();
        let mapper = f.into_native::<((PackageDescription,), Either![Null, SourceDescription])>();

        let mut visited = BTreeSet::new();
        for package in lock.packages.iter().filter(|p| p.source.is_none()) {
            find_packages_by_source(s.clone(), &lock, package, &mut patches, &mut visited, &mapper)?;
        }

        {
            info!("writing it to patch section");
            let toml = fs::read_to_string("./Cargo.toml").map_err(to_runtime)?;

            let mut document = toml.parse::<Document>().map_err(to_runtime)?;
            let table = document.as_table_mut();

            if !table.contains_table("patch") {
                table.insert("patch", Item::Table(Table::new()));
            }
            let patch = table.get_mut("patch").unwrap().as_table_mut().unwrap();
            patch.set_implicit(true);

            for (pkg, new_source) in &patches {
                if !patch.contains_table(&pkg.source) {
                    patch.insert(&pkg.source, Item::Table(Table::new()));
                }
                let source = patch.get_mut(&pkg.source).unwrap().as_table_mut().unwrap();
                let mut override_source = InlineTable::new();

                if let Some(git) = &new_source.git {
                    override_source.insert("git", git.into());
                }
                if let Some(branch) = &new_source.branch {
                    override_source.insert("branch", branch.into());
                }
                if let Some(path) = &new_source.path {
                    override_source.insert("path", path.into());
                }

                source.insert(&pkg.name, Item::Value(Value::InlineTable(override_source)));
            }

            fs::write("./Cargo.toml", document.to_string()).map_err(to_runtime)?;
        }
        checked_lockfiles.insert(lock.to_string());
    }

    Ok(())
}

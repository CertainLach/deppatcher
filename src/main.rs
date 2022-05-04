use std::{
    env::current_dir,
    fs,
    io::{stdin, Read},
    path::{Path, PathBuf},
    result,
};

use clap::Parser;
use jrsonnet_cli::{ConfigureState, GeneralOpts, InputOpts};
use jrsonnet_evaluator::{
    error::{Error, Result},
    function::{builtin, FuncVal},
    throw_runtime,
    typed::Either2,
    typed::{Null, Typed},
    Either, ObjValue, ObjValueBuilder, State, Val,
};
use toml_edit::{Document, InlineTable, Item, Table, TableLike, Value};
use tracing::info;

trait ToRuntime<T> {
    fn run_err(self) -> Result<T>;
}
impl<T, E> ToRuntime<T> for result::Result<T, E>
where
    E: ToString,
{
    fn run_err(self) -> Result<T> {
        Ok(self.map_err(|e| Error::RuntimeError(e.to_string().into()))?)
    }
}

#[derive(Typed, Debug, Clone, PartialEq, Eq)]
struct DirectSource {
    version: Option<String>,

    path: Option<String>,

    git: Option<String>,
    rev: Option<String>,
    tag: Option<String>,
    branch: Option<String>,

    registry: Option<String>,
}
impl DirectSource {
    fn read(table: &dyn TableLike) -> Self {
        let get = |s: &str| table.get(s).and_then(Item::as_str).map(ToOwned::to_owned);
        Self {
            version: get("version"),
            path: get("path"),
            git: get("git"),
            rev: get("rev"),
            tag: get("tag"),
            branch: get("branch"),
            registry: get("registry"),
        }
    }
    fn write(&self, table: &mut dyn TableLike) {
        let mut set = |s: &str, v: &Option<String>| {
            if let Some(v) = v {
                table.insert(s, Item::Value(v.into()));
            } else {
                table.remove(s);
            }
        };
        set("version", &self.version);
        set("path", &self.path);
        set("git", &self.git);
        set("rev", &self.rev);
        set("tag", &self.tag);
        set("branch", &self.branch);
        set("registry", &self.registry);
    }
    fn to_table(&self) -> InlineTable {
        let mut table = InlineTable::new();
        self.write(&mut table);
        table
    }
}

#[derive(Typed, Debug, Clone)]
struct DirectInput {
    name: String,
    package: String,
    source: DirectSource,
    #[typed(rename = "originalSource")]
    original_source: DirectSource,
}

type Key = Vec<String>;

type Mutator = dyn Fn(State, DirectInput) -> Result<Either![Null, DirectSource]>;

fn patch_dep(
    s: State,
    originals: &mut Item,
    key: &mut Key,
    dep: &mut dyn TableLike,
    mutator: &Mutator,
) -> Result<()> {
    let name = key.iter().last().unwrap().as_str();
    let package = dep
        .get("package")
        .and_then(|v| v.as_str())
        .unwrap_or(name)
        .to_owned();
    let source = DirectSource::read(dep);
    let (had_original, original_source) = get_item(&originals, key.iter().map(String::as_str))
        .and_then(|i| i.as_table_like())
        .map(DirectSource::read)
        .map(|v| (true, v))
        .unwrap_or_else(|| (false, source.clone()));

    let input = DirectInput {
        name: name.to_owned(),
        package: package.clone(),
        source: source.clone(),
        original_source: original_source.clone(),
    };
    let new_source = if let Either2::B(new_source) = mutator(s, input.clone())? {
        new_source
    } else {
        return Ok(());
    };

    if new_source == source {
        return Ok(());
    }

    info!("rewrite {} => {}", source.to_table(), new_source.to_table());

    let originals = originals.as_table_mut().expect("is table checked");
    if !had_original {
        let name = key.pop().unwrap();
        key.push(package);
        set_table(
            originals,
            key,
            Item::Value(Value::InlineTable(source.to_table())),
        );
        key.pop();
        key.push(name);
    } else if original_source == new_source {
        set_table(originals, key, Item::None);
    }

    new_source.write(dep);

    Ok(())
}

fn patch_dep_table(
    s: State,
    originals: &mut Item,
    key: &mut Key,
    deps: &mut Table,
    mutator: &Mutator,
) -> Result<()> {
    for (d, table) in deps
        .iter_mut()
        .flat_map(|(k, t)| t.as_table_like_mut().map(|t| (k, t)))
    {
        key.push(d.get().to_owned());
        patch_dep(s.clone(), originals, key, table, mutator)?;
        key.pop();
    }
    Ok(())
}

fn patch_target_table(
    s: State,
    originals: &mut Item,
    key: &mut Key,
    target: &mut Table,
    mutator: &Mutator,
) -> Result<()> {
    for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(deps) = target.get_mut(kind).and_then(|t| t.as_table_mut()) {
            key.push(kind.to_owned());
            patch_dep_table(s.clone(), originals, key, deps, mutator)?;
            key.pop();
        }
    }
    Ok(())
}

fn get_item<'t, 'k>(table: &'t Item, key: impl IntoIterator<Item = &'k str>) -> Option<&'t Item> {
    key.into_iter()
        .try_fold(table, |table, key| table.as_table_like()?.get(key))
}
fn set_table<'t, 'k>(mut table: &mut Table, key: &Key, value: Item) {
    let (last, path) = key.split_last().unwrap();

    for frag in path {
        table = if table.contains_table(&frag) {
            let old = table
                .get_mut(&frag)
                .expect("just tested")
                .as_table_mut()
                .expect("just tested");
            old.set_implicit(true);
            old
        } else {
            let mut new = Table::new();
            new.set_implicit(true);
            table.insert(&frag, Item::Table(new));
            table
                .get_mut(frag)
                .expect("just added")
                .as_table_mut()
                .expect("just added")
        }
    }
    table.insert(&last, value);
}

fn freeze(path: &Path) -> Result<()> {
    let toml = fs::read_to_string(path).run_err()?;
    let mut doc: Document = toml.parse().run_err()?;
    set_table(
        doc.as_table_mut(),
        &vec![
            "package".to_owned(),
            "metadata".to_owned(),
            "deppatcher".to_owned(),
            "originals".to_owned(),
        ],
        Item::None,
    );
    let toml = doc.to_string();
    fs::write(path, toml).run_err()?;
    Ok(())
}

fn patch(s: State, path: &Path, mutator: &Mutator) -> Result<()> {
    let toml = fs::read_to_string(path).run_err()?;
    let mut doc: Document = toml.parse().run_err()?;
    let mut originals = get_item(
        doc.as_item(),
        ["package", "metadata", "deppatcher", "originals"],
    )
    .cloned()
    .unwrap_or_else(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });

    if !originals.is_table() {
        throw_runtime!("originals should be table");
    }

    let table = doc.as_table_mut();

    let mut key = Vec::new();
    patch_target_table(s.clone(), &mut originals, &mut key, table, mutator)?;
    if let Some(table) = table.get_mut("target").and_then(|t| t.as_table_mut()) {
        key.push("target".to_owned());
        for (k, table) in table
            .iter_mut()
            .flat_map(|(k, t)| t.as_table_mut().map(|t| (k, t)))
        {
            key.push(k.get().to_owned());
            patch_target_table(s.clone(), &mut originals, &mut key, table, mutator)?;
            key.pop();
        }
        key.pop();
    }

    set_table(
        table,
        &vec![
            "package".to_owned(),
            "metadata".to_owned(),
            "deppatcher".to_owned(),
            "originals".to_owned(),
        ],
        originals,
    );

    let toml = doc.to_string();
    fs::write(path, toml).run_err()?;

    Ok(())
}

/// Mass rewriter of Cargo.toml files
#[derive(Parser)]
#[clap(author)]
enum Opts {
    /// Rewrite package sources using specified rule
    Patch {
        #[clap(flatten)]
        input: InputOpts,
        #[clap(flatten)]
        general: GeneralOpts,
    },
    /// Revert back to original packages version, alias to `patch -e 'function(p) p.originalSource'`
    Revert,
    /// Remove all saved original packages
    Freeze,
}

#[builtin]
fn load_paths(s: State, tree: String) -> Result<ObjValue> {
    let tree = PathBuf::from(tree);
    let mut command = cargo_metadata::MetadataCommand::new();
    command.no_deps();
    command.current_dir(tree);
    let metadata = command.exec().run_err()?;

    let mut out = ObjValueBuilder::new();
    for package in &metadata.packages {
        let path = package.manifest_path.parent().unwrap();
        out.member(package.name.clone().into())
            .value(s.clone(), Val::Str(path.to_string().into()))?;
    }
    Ok(out.build())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut opts = Opts::parse();
    if let Opts::Revert = opts {
        opts = Opts::parse_from(["deppatcher", "patch", "-e", "function(p) p.originalSource"])
    };
    match opts {
        Opts::Freeze => {
            for entry in walkdir::WalkDir::new(current_dir().run_err()?) {
                let entry = entry.run_err()?;
                if entry.file_type().is_file() && entry.path().ends_with("Cargo.toml") {
                    info!("freezing {}", entry.path().display());
                    freeze(&entry.path())?;
                }
            }
        }
        Opts::Revert => unreachable!("this is alias"),
        Opts::Patch { input, general } => {
            let s = State::default();

            let mut dpp = ObjValueBuilder::new();
            dpp.member("loadPaths".into()).value(
                s.clone(),
                Val::Func(FuncVal::StaticBuiltin(load_paths::INST)),
            )?;
            let dpp = dpp.build();

            {
                s.settings_mut()
                    .globals
                    .insert("dpp".into(), Val::Obj(dpp.clone()));
            }

            general.configure(&s)?;

            let mutator = if input.exec {
                s.evaluate_snippet_raw(PathBuf::from("<cmdline>").into(), input.input.into())?
            } else if input.input.as_str() == "-" {
                let mut code = String::new();
                stdin().read_to_string(&mut code).run_err()?;
                s.evaluate_snippet_raw(PathBuf::from("<stdin>").into(), code.into())?
            } else {
                s.evaluate_file_raw(&PathBuf::from(input.input))?
            };
            let mutator = FuncVal::from_untyped(mutator, s.clone())?;
            let mutator = mutator.into_native::<((DirectInput,), Either![Null, DirectSource])>();

            for entry in walkdir::WalkDir::new(current_dir().run_err()?) {
                let entry = entry.run_err()?;
                if entry.file_type().is_file() && entry.path().ends_with("Cargo.toml") {
                    info!("patching {}", entry.path().display());
                    patch(s.clone(), &entry.path(), &mutator)?;
                }
            }
        }
    }

    Ok(())
}

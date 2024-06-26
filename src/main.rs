#![warn(clippy::pedantic, clippy::nursery)]
#![allow(clippy::needless_pass_by_value)]
#![doc = include_str!("../README.md")]

use std::{
	collections::{BTreeMap, HashSet},
	env::current_dir,
	fs,
	io::{stdin, Read},
	path::{Path, PathBuf},
	result,
	string::ToString,
};

use clap::Parser;
use guppy::graph::{DependencyDirection, ExternalSource, GitReq};
use jrsonnet_cli::{InputOpts, StdOpts};
use jrsonnet_evaluator::{
	bail,
	error::{ErrorKind, Result},
	function::{builtin, CallLocation, FuncVal},
	parser::Source,
	typed::{Either2, NativeFn, Null, Typed},
	val::StrValue,
	ContextBuilder, ContextInitializer, Either, ObjValue, ObjValueBuilder, State, Thunk, Val,
};
use jrsonnet_gcmodule::Trace;
use toml_edit::{Document, Formatted, InlineTable, Item, Table, TableLike, Value};
use tracing::info;

trait ToRuntime<T> {
	fn run_err(self) -> Result<T>;
}
impl<T, E> ToRuntime<T> for result::Result<T, E>
where
	E: ToString,
{
	fn run_err(self) -> Result<T> {
		Ok(self.map_err(|e| ErrorKind::RuntimeError(e.to_string().into()))?)
	}
}

#[derive(Typed, Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DirectSource {
	/// Package version, None if package is obtained not from registry
	pub version: Option<String>,
	/// None for default registry
	pub registry: Option<String>,

	/// Full path to package directory
	/// (not to workspace containing this package)
	pub path: Option<String>,

	pub git: Option<String>,
	pub rev: Option<String>,
	pub tag: Option<String>,
	pub branch: Option<String>,
	pub workspace: Option<bool>,
}
impl DirectSource {
	fn read(table: &dyn TableLike) -> Self {
		let get = |s: &str| table.get(s).and_then(Item::as_str).map(ToOwned::to_owned);
		let get_bool = |s: &str| table.get(s).and_then(Item::as_bool);
		Self {
			version: get("version"),
			path: get("path"),
			git: get("git"),
			rev: get("rev"),
			tag: get("tag"),
			branch: get("branch"),
			registry: get("registry"),
			workspace: get_bool("workspace"),
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
		let mut set_bool = |s: &str, v: &Option<bool>| {
			if let Some(v) = v {
				table.insert(s, Item::Value((*v).into()));
			} else {
				table.remove(s);
			}
		};
		set_bool("workspace", &self.workspace);
	}
	fn to_table(&self) -> InlineTable {
		let mut table = InlineTable::new();
		self.write(&mut table);
		table
	}
}

#[derive(Typed, Debug, Clone, PartialOrd, Ord, PartialEq, Eq)]
pub struct DirectInput {
	/// Name with which this package was referenced in `Cargo.toml`
	/// ```toml
	/// name = {...}
	/// ```
	pub name: String,
	/// Either referenced name, or explicitly specified package
	/// ```toml
	/// package = "1.0"
	/// name = { package = "package", version = "1.0" }
	/// ```
	pub package: String,
	/// Source, with which this package is currently referenced
	pub source: DirectSource,
	/// Backed up package source
	#[typed(rename = "originalSource")]
	pub original_source: DirectSource,
}

type Key = Vec<String>;

type Mutator = dyn Fn(DirectInput) -> Result<Either![Null, DirectSource]>;

fn patch_dep(
	originals: &mut Item,
	key: &mut Key,
	dep: &mut dyn TableLike,
	mutator: &Mutator,
) -> Result<()> {
	let force = false;
	let name = key.iter().last().unwrap().as_str();
	let package = dep
		.get("package")
		.and_then(Item::as_str)
		.unwrap_or(name)
		.to_owned();
	let source = DirectSource::read(dep);
	let (had_original, original_source) = get_item(originals, key.iter().map(String::as_str))
		.and_then(Item::as_table_like)
		.map(DirectSource::read)
		.map_or_else(|| (false, source.clone()), |v| (true, v));

	let input = DirectInput {
		name: name.to_owned(),
		package: package.clone(),
		source: source.clone(),
		original_source: original_source.clone(),
	};
	let new_source = if let Either2::B(new_source) = mutator(input)? {
		new_source
	} else if force {
		source.clone()
	} else {
		return Ok(());
	};

	if new_source == source && !force {
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
	originals: &mut Item,
	key: &mut Key,
	deps: &mut Table,
	mutator: &Mutator,
	force_inline: bool,
) -> Result<()> {
	for (d, table) in deps
		.iter_mut()
		.filter_map(|(k, t)| t.is_table_like().then_some((k, t)))
	{
		key.push(d.get().to_owned());
		patch_dep(
			originals,
			key,
			table.as_table_like_mut().expect("is table checked"),
			mutator,
		)?;
		if force_inline {
			if let Some(astable) = table.as_table_mut() {
				astable.set_implicit(true);
				*table = Item::Value(Value::InlineTable(astable.clone().into_inline_table()));
			}
			if let Some(astable) = table.as_inline_table_mut() {
				if astable.len() == 1 {
					astable.set_dotted(true);
				}
			}
		}
		let astable = table.as_table_like().expect("is table checked");
		if astable.len() == 1 {
			if let Some(version) = table.get("version") {
				*table = version.clone();
			}
		}
		key.pop();
	}
	for (d, table) in deps
		.iter_mut()
		.filter_map(|(k, t)| t.is_str().then_some((k, t)))
	{
		let version = Value::String(Formatted::new(
			table.as_str().expect("iterating over strings").to_owned(),
		));
		key.push(d.get().to_owned());
		let mut tmp = InlineTable::new();
		tmp.insert("version", version);
		// Assuming no one will use `package=version` syntax, when one prefers non-inline table for dependencies (actual psychos).
		*table = Item::Value(Value::InlineTable(tmp));

		patch_dep(
			originals,
			key,
			table.as_table_like_mut().expect("is table checked"),
			mutator,
		)?;
		if force_inline {
			if let Some(astable) = table.as_table_mut() {
				astable.set_implicit(true);
				*table = Item::Value(Value::InlineTable(astable.clone().into_inline_table()));
			}
			if let Some(astable) = table.as_inline_table_mut() {
				if astable.len() == 1 {
					astable.set_dotted(true);
				}
			}
		}
		let astable = table.as_table_like().expect("is table checked");
		if astable.len() == 1 {
			if let Some(version) = table.get("version") {
				*table = version.clone();
			}
		}
		key.pop();
	}
	Ok(())
}

fn patch_target_table(
	originals: &mut Item,
	key: &mut Key,
	target: &mut Table,
	mutator: &Mutator,
	force_inline: bool,
) -> Result<()> {
	for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
		if let Some(deps) = target.get_mut(kind).and_then(Item::as_table_mut) {
			key.push(kind.to_owned());
			patch_dep_table(originals, key, deps, mutator, force_inline)?;
			key.pop();
		}
	}
	Ok(())
}

fn patch_root_table(
	originals: &mut Item,
	key: &mut Key,
	table: &mut Table,
	mutator: &Mutator,
	force_inline: bool,
) -> Result<()> {
	patch_target_table(originals, key, table, mutator, force_inline)?;
	if let Some(table) = table.get_mut("target").and_then(Item::as_table_mut) {
		key.push("target".to_owned());
		for (k, table) in table
			.iter_mut()
			.filter_map(|(k, t)| t.as_table_mut().map(|t| (k, t)))
		{
			key.push(k.get().to_owned());
			patch_target_table(originals, key, table, mutator, force_inline)?;
			key.pop();
		}
		key.pop();
	}
	Ok(())
}

fn get_item<'t, 'k>(table: &'t Item, key: impl IntoIterator<Item = &'k str>) -> Option<&'t Item> {
	key.into_iter()
		.try_fold(table, |table, key| table.as_table_like()?.get(key))
}
fn set_table(mut table: &mut Table, key: &Key, value: Item) {
	let (last, path) = key.split_last().unwrap();

	for frag in path {
		table = if table.contains_table(frag) {
			let old = table
				.get_mut(frag)
				.expect("just tested")
				.as_table_mut()
				.expect("just tested");
			old.set_implicit(true);
			old
		} else {
			let mut new = Table::new();
			new.set_implicit(true);
			table.insert(frag, Item::Table(new));
			table
				.get_mut(frag)
				.expect("just added")
				.as_table_mut()
				.expect("just added")
		}
	}
	table.insert(last, value);
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
	set_table(
		doc.as_table_mut(),
		&vec![
			"workspace".to_owned(),
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

fn patch(path: &Path, mutator: &Mutator, force_inline: bool) -> Result<()> {
	let toml = fs::read_to_string(path).run_err()?;
	let mut doc: Document = toml.parse().run_err()?;
	let metadata_root = if doc.contains_key("package") {
		"package"
	} else {
		"workspace"
	};
	let mut originals = get_item(
		doc.as_item(),
		[metadata_root, "metadata", "deppatcher", "originals"],
	)
	.cloned()
	.unwrap_or_else(|| {
		let mut table = Table::new();
		table.set_implicit(true);
		Item::Table(table)
	});

	if !originals.is_table() {
		bail!("originals should be table");
	}

	let table = doc.as_table_mut();

	let mut key = Vec::new();
	patch_root_table(&mut originals, &mut key, table, mutator, force_inline)?;
	if let Some(table) = table.get_mut("workspace").and_then(Item::as_table_mut) {
		key.push("workspace".to_owned());
		patch_root_table(&mut originals, &mut key, table, mutator, force_inline)?;
		key.pop();
	}
	assert_eq!(key.len(), 0);

	set_table(
		table,
		&vec![
			metadata_root.to_owned(),
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
#[allow(clippy::large_enum_variant)]
#[derive(Parser)]
#[clap(author, disable_version_flag = true)]
enum Opts {
	/// Rewrite package sources using specified rule
	Patch {
		/// Format dependencies as inline table
		#[clap(long)]
		force_inline: bool,

		#[clap(flatten)]
		input: InputOpts,
		#[clap(flatten)]
		std: StdOpts,
	},
	/// Generate `[patch]` section in workspace Cargo.toml
	/// Operates on `cargo metadata`, slower, but allows to rewrite other package dependencies
	SoftPatch {
		#[clap(flatten)]
		input: InputOpts,
		#[clap(flatten)]
		std: StdOpts,
	},
	/// Revert back to original packages version
	Revert,
	/// Rewrite all package sources, to ones defined in specified workspace
	Link {
		/// Workspace to link
		workspace: String,
		/// Use soft-patch instead of patch
		#[clap(long)]
		soft: bool,
		#[clap(long)]
		by_version: bool,
	},
	/// Remove all saved original packages
	Freeze,
}

#[builtin]
fn load_paths(loc: CallLocation, workspace: String) -> Result<ObjValue> {
	let mut path = match loc.0 {
		Some(loc) => loc
			.0
			.source_path()
			.path()
			.map_or(current_dir().expect("no current dir?"), Path::to_path_buf),
		None => bail!("only callable from jsonnet"),
	};
	path.push(workspace);

	let mut command = cargo_metadata::MetadataCommand::new();
	command.no_deps();
	command.current_dir(path);
	let metadata = command.exec().run_err()?;

	let mut out = ObjValueBuilder::new();
	for package in &metadata.packages {
		let path = package.manifest_path.parent().unwrap();
		out.field(package.name.clone())
			.value(Val::Str(StrValue::Flat(path.to_string().into())));
	}
	Ok(out.build())
}

#[builtin]
fn load_locked(loc: CallLocation, lockfile: String) -> Result<ObjValue> {
	let mut path = match loc.0 {
		Some(loc) => loc
			.0
			.source_path()
			.path()
			.map_or(current_dir().expect("no current dir?"), Path::to_path_buf),
		None => bail!("only callable from jsonnet"),
	};
	path.push(lockfile);

	let lockfile = cargo_lock::Lockfile::load(path).run_err()?;
	let mut out = ObjValueBuilder::new();
	for dep in lockfile.packages {
		if dep.source.is_some() {
			continue;
		}
		out.field(dep.name.to_string())
			.value(Val::Str(StrValue::Flat(dep.version.to_string().into())));
	}
	Ok(out.build())
}

#[derive(Trace)]
struct DppContextInitializer;
impl ContextInitializer for DppContextInitializer {
	fn populate(&self, _for_file: Source, builder: &mut ContextBuilder) {
		let mut dpp = ObjValueBuilder::new();
		dpp.field("loadPaths")
			.value(Val::Func(FuncVal::StaticBuiltin(load_paths::INST)));
		dpp.field("loadLocked")
			.value(Val::Func(FuncVal::StaticBuiltin(load_locked::INST)));
		let dpp = dpp.build();
		builder.bind("dpp", Thunk::evaluated(Val::from(dpp)));
	}

	fn as_any(&self) -> &dyn std::any::Any {
		self
	}
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn main() -> Result<()> {
	tracing_subscriber::fmt::init();

	let mut opts = Opts::parse();
	if matches!(opts, Opts::Revert) {
		opts = Opts::parse_from(["deppatcher", "patch", "-e", "function(p) p.originalSource"]);
	} else if let Opts::Link {
		workspace,
		soft,
		by_version,
	} = opts
	{
		let mut ext = String::new();
		ext.push_str("linkTo=");
		ext.push_str(&workspace);
		opts = Opts::parse_from([
			"deppatcher",
			if soft { "soft-patch" } else { "patch" },
			"--ext-str",
			&ext,
			"-e",
			if by_version {
				r"
					local linkFrom = dpp.loadLocked('./Cargo.lock');
					local _linkTo = dpp.loadLocked(std.extVar('linkTo'));
					local linkTo = {
						[k]: _linkTo[k]
						for k in std.objectFields(_linkTo)
						if !(k in linkFrom)
					};
					function(pkg) if
						std.objectHas(linkTo, pkg.package)
						&& !std.get(pkg.source, 'workspace', false)
					then {
						version: linkTo[pkg.package],
					}
				"
			} else {
				r"
					local linkTo = dpp.loadPaths(std.extVar('linkTo'));
					function(pkg) if
						std.objectHas(linkTo, pkg.package)
						&& !std.get(pkg.source, 'workspace', false)
					then {
						path: linkTo[pkg.package],
					}
				"
			},
		]);
	}
	match opts {
		Opts::Freeze => {
			for entry in walkdir::WalkDir::new(current_dir().run_err()?) {
				let entry = entry.run_err()?;
				if entry.file_type().is_file() && entry.path().ends_with("Cargo.toml") {
					info!("freezing {}", entry.path().display());
					freeze(entry.path())?;
				}
			}
		}
		Opts::Revert | Opts::Link { .. } => unreachable!("this is alias"),
		Opts::Patch {
			input,
			force_inline,
			std,
		} => {
			let s = State::default();

			s.set_context_initializer((
				std.context_initializer(&s)?.expect("nostd is not working"),
				DppContextInitializer,
			));

			let mutator = if input.exec {
				s.evaluate_snippet("<cmdline>".to_string(), input.input)?
			} else if input.input.as_str() == "-" {
				let mut code = String::new();
				stdin().read_to_string(&mut code).run_err()?;
				s.evaluate_snippet("<stdin>".to_string(), code)?
			} else {
				s.import(PathBuf::from(input.input))?
			};
			let mutator =
				<NativeFn<((DirectInput,), Either![Null, DirectSource])>>::from_untyped(mutator)?;

			for entry in walkdir::WalkDir::new(current_dir().run_err()?) {
				let entry = entry.run_err()?;
				if entry.file_type().is_file() && entry.path().ends_with("Cargo.toml") {
					info!("patching {}", entry.path().display());
					patch(entry.path(), &*mutator, force_inline)?;
				}
			}
		}
		Opts::SoftPatch { input, std } => {
			let s = State::default();

			s.set_context_initializer((
				std.context_initializer(&s)?.expect("nostd is not working"),
				DppContextInitializer,
			));

			let mutator = if input.exec {
				s.evaluate_snippet("<cmdline>".to_string(), input.input)?
			} else if input.input.as_str() == "-" {
				let mut code = String::new();
				stdin().read_to_string(&mut code).run_err()?;
				s.evaluate_snippet("<stdin>".to_string(), code)?
			} else {
				s.import(PathBuf::from(input.input))?
			};
			let mutator =
				<NativeFn<((DirectInput,), Either![Null, DirectSource])>>::from_untyped(mutator)?;

			let guppy = guppy::MetadataCommand::new().exec().run_err()?;
			let graph = guppy.build_graph().run_err()?;

			let mut output = <BTreeMap<DirectInput, DirectSource>>::new();

			let mut visited = HashSet::new();
			let mut to_visit = graph
				.resolve_workspace()
				.root_packages(DependencyDirection::Forward)
				.map(|p| p.id())
				.collect::<Vec<_>>();
			while !to_visit.is_empty() {
				for package in std::mem::take(&mut to_visit) {
					// Somehow, this graph is cyclic
					if !visited.insert(package) {
						continue;
					}

					let pkg = graph
						.packages()
						.find(|i| i.id() == package)
						.expect("bad graph");
					for ele in pkg.direct_links() {
						if !ele.normal().is_present() && !ele.build().is_present() {
							continue;
						}
						let to = ele.to();
						let source = ele.to().source();
						let es = source.parse_external();
						let git = match source.parse_external() {
							Some(ExternalSource::Git {
								repository,
								req,
								resolved,
							}) => Some((repository.to_string(), req, resolved)),
							_ => None,
						};
						let ds = DirectSource {
							version: Some(to.version().to_string()),
							registry: match es {
								Some(ExternalSource::Registry(r)) => Some(r.to_string()),
								_ => None,
							},
							path: source.local_path().map(ToString::to_string),
							git: git.as_ref().map(|(r, _, _)| r.to_string()),
							rev: git.as_ref().and_then(|(_, e, _)| match e {
								GitReq::Rev(e) => Some((*e).to_string()),
								_ => None,
							}),
							tag: git.as_ref().and_then(|(_, e, _)| match e {
								GitReq::Tag(t) => Some((*t).to_string()),
								_ => None,
							}),
							branch: git.as_ref().and_then(|(_, e, _)| match e {
								GitReq::Branch(b) => Some((*b).to_string()),
								_ => None,
							}),
							workspace: None,
						};

						let input = DirectInput {
							package: to.name().to_string(),
							name: to.name().to_string(),
							// Not supported
							original_source: ds.clone(),
							source: ds.clone(),
						};
						if output.contains_key(&input) {
							continue;
						}

						match (*mutator)(input.clone())? {
							Either2::A(_) => {}
							Either2::B(r) => {
								if r != ds {
									output.insert(input.clone(), r);
								}
							}
						}
						to_visit.push(ele.to().id());
					}
				}
			}

			let mut table = Document::new();
			table.insert_formatted(&toml_edit::Key::new("patch"), Item::Table(Table::new()));
			let patch_table = table
				.get_mut("patch")
				.expect("just inserted")
				.as_table_mut()
				.expect("table like");
			patch_table.set_implicit(true);

			for (k, v) in output {
				let source = if let Some(reg) = &k.source.registry {
					if reg == "https://github.com/rust-lang/crates.io-index" {
						"crates-io".to_string()
					} else {
						bail!("no support for custom registries")
					}
				} else if let Some(git) = &k.source.git {
					git.to_string()
				} else if k.source.path.is_some() {
					eprintln!("path exists {:?}", k.source);
					continue;
				} else {
					bail!("unsupported source: {:?}", k.source)
				};
				let source_table = patch_table
					.entry(&source)
					.or_insert(Item::Table(Table::new()))
					.as_table_mut()
					.expect("table like");
				source_table.set_implicit(false);
				let item_table = source_table
					.entry(&k.name)
					.or_insert(Item::Value(Value::InlineTable(InlineTable::new())))
					.as_table_like_mut()
					.expect("table like");
				v.write(item_table);
			}

			println!("{table}");
		}
	}

	Ok(())
}

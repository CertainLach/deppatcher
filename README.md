# deppatcher

Utility for mass rewriting of Cargo.toml files

## Usage

Rule - jsonnet function, which receives package description (see [`DirectInput`]), and returns package source (see [`DirectSource`])

I.e you want to rewrite all usages of package `evm` using git repo, you can use this rule:

```jsonnet
function(pkg) if pkg.package == "evm" then {
	git: "https://github.com/CertainLach/evm"
}
```

To execute this rule, either write `deppatcher patch -e "rule"`, or save it to file, and then `deppatcher patch file.jsonnet`. Patch command receives same arguments as jsonnet interpreter

To quickly override all used packages with ones defined in other workspace use

```shell
deppatcher link /home/lach/build/my-evm-fork
```

After rewrite, original package source will be stored in `Cargo.toml`, and can be either restored (`deppatcher revert`), or removed (`deppatcher freeze`)

## Usage with local workspaces

For path overrides, cargo expects you to provide full path to dependency, and in case of workspace - path to dependency in this workspace

To simplify things, there is `dpp.loadPaths` helper available, that receives a path to workspace (relative to rule file), and returns object, which maps packages in that workspace to absolute paths to them

I.e for [jrsonnet](https://github.com/CertainLach/jrsonnet) it will return
```jsonnet
{
	jrsonnet: '/code/cmds/jrsonnet',
	jsonnet: '/code/bindings/jsonnet'
	'jrsonnet-evaluator': '/code/crates/evaluator',
	'jrsonnet-parser': '/code/crates/parser',
	'jrsonnet-interner': '/code/crates/interner',
	...
}
```
Where `/code/` - absolute path to repository tree

## Example scenarios

1. You use substrate, you can only depend on git version, and you can't just specify master branch

In repository you have a lot of similar lines:
```toml
sp-api = { default-features = false, git = "https://github.com/paritytech/substrate", branch = "polkadot-v0.9.18" }
```
When you need to update to next version of this dependency, you search&replace a lot of times, this is annoying, and doesn't always work (I.e if you want to have your own versioning in your forks)
```toml
sp-api = { default-features = false, git = "https://github.com/paritytech/substrate", branch = "polkadot-v0.9.19" }
```

With deppatcher you should just write update rule, like so:
```jsonnet
function(pkg) if pkg.source.git == "https://github.com/paritytech/substrate" then pkg.source {
	branch: "polkadot-v0.9.19"
}
```

deppatcher stores original versions of packages for revert, you need to run `deppatcher freeze` to remove them

2. You depend on <https://github.com/paritytech/frontier>, this repo has a lot of modules, you need to temporary use local fork.

Using `[patch]` and/or manually specifying `path` will work, however, those can only point to virtual manifests, you can't type
```toml
fp-consensus = { path = "~/my-frontier-fork" }
```
You should type path to dependency in this repository clone
```toml
fp-consensus = { path = "~/my-frontier-fork/primitives/consensus" }
```

deppatcher comes to rescue!
```jsonnet
local frontier = dpp.loadPaths('/home/lach/work/substrate/frontier');
function(pkg) if std.objectHas(frontier, pkg.package) then {
	path: frontier[pkg.package],
}
```

Or by using alias, which does exactly like code on top under hood:
```shell
deppatcher link /home/lach/work/substrate/frontier
```

When you need to switch everything back - use `deppatcher revert` command

## Alternatives
<https://github.com/bkchr/diener> - very limited, you can't update non-substrate dependency (i.e frontier or forked substrate), revert part of patch, or perform any other non-trivial operation. Everything you can do with diener - you also can do with deppatcher

//! Graph loading: runs .ninja parsing and constructs the build graph from it.

use crate::{
    canon::{canonicalize_path, to_owned_canon_path},
    db,
    densemap::DenseMap,
    eval::{self, EvalPart, EvalString},
    graph::{self, BuildDeps, BuildId, BuildIns, BuildOuts, FileId, FileLoc, FilenameResolver, RspFile},
    parse::{self, Statement, VarList},
    scanner,
    smallmap::SmallMap,
    trace
};
use anyhow::{anyhow, bail};
use std::{borrow::Cow, collections::HashMap, ops::Deref};
use std::path::PathBuf;
use std::path::Path;

/// A variable lookup environment for magic $in/$out variables.
struct BuildImplicitVars<'a> {
    resolver: &'a dyn FilenameResolver,
    build:    &'a LazyBuild,
}
impl<'a> BuildImplicitVars<'a> {
    fn file_list(&self, ids: &[FileId], sep: char) -> String {
        let mut out = String::new();
        for &id in ids {
            if !out.is_empty() {
                out.push(sep);
            }
            out.push_str(&self.resolver.lookup_filename(id));
        }
        out
    }
}
impl<'a> eval::Env for BuildImplicitVars<'a> {
    fn get_var(&self, var: &str) -> Option<EvalString<Cow<str>>> {
        let string_to_evalstring =
            |s: String| Some(EvalString::new(vec![EvalPart::Literal(Cow::Owned(s))]));
        match var {
            "in" => string_to_evalstring(self.file_list(self.build.explicit_ins(), ' ')),
            "in_newline" => string_to_evalstring(self.file_list(self.build.explicit_ins(), '\n')),
            "out" => string_to_evalstring(self.file_list(self.build.explicit_outs(), ' ')),
            "out_newline" => string_to_evalstring(self.file_list(self.build.explicit_outs(), '\n')),
            _ => None,
        }
    }
}

/// Internal state used while loading.
#[derive(Default)]
pub struct Loader {
    pub graph: graph::Graph,
    pub lazy_builds: DenseMap<BuildId, LazyBuild>,
    pub env: eval::Vars,
    pub default: Vec<FileId>,
    /// rule name -> list of (key, val)
    rules: HashMap<String, VarList>,
    pools: SmallMap<String, usize>,
}

impl Loader {
    pub fn new() -> Self {
        let mut loader = Loader::default();

        loader.rules.insert("phony".to_owned(), SmallMap::default());

        loader
    }

    /// Convert a path string to a FileId.
    fn path(&mut self, mut path: String) -> FileId {
        // Perf: this is called while parsing build.ninja files.  We go to
        // some effort to avoid allocating in the common case of a path that
        // refers to a file that is already known.
        canonicalize_path(&mut path);
        self.graph.files.id_from_canonical(path)
    }

    fn evaluate_path(&mut self, path: EvalString<&str>, envs: &[&dyn eval::Env]) -> FileId {
        self.path(path.evaluate(envs))
    }

    fn evaluate_paths(
        &mut self,
        paths: Vec<EvalString<&str>>,
        envs: &[&dyn eval::Env],
    ) -> Vec<FileId> {
        paths
            .into_iter()
            .map(|path| self.evaluate_path(path, envs))
            .collect()
    }

    fn add_build(
        &mut self,
        filename: std::rc::Rc<PathBuf>,
        env: &eval::Vars,
        b: parse::Build,
    ) -> anyhow::Result<()> {
        let ins = graph::BuildIns {
            ids: self.evaluate_paths(b.ins, &[&b.vars, env]),
            explicit: b.explicit_ins,
            implicit: b.implicit_ins,
            order_only: b.order_only_ins,
            // validation is implied by the other counts
        };
        let outs = graph::BuildOuts {
            ids: self.evaluate_paths(b.outs, &[&b.vars, env]),
            explicit: b.explicit_outs,
        };

        let rule = match self.rules.get(b.rule) {
            Some(r) => r,
            None => bail!("unknown rule {:?}", b.rule),
        };

        let mut lazy_build = {
            let loc = graph::FileLoc {
                    filename,
                    line: b.line,
                };
            let vars = b.vars;
            LazyBuild {
                deps: BuildDeps::new(loc, ins, outs),
                rule: rule.clone(),
                vars,
            }
        };

        let new_id = self.lazy_builds.next_id();
        for &id in &lazy_build.ins.ids {
            self.graph.files.by_id[id].dependents.push(new_id);
        }
        let mut fixup_dups = false;
        for &id in &lazy_build.outs.ids {
            let f = &mut self.graph.files.by_id[id];
            match f.input {
                Some(prev) if prev == new_id => {
                    fixup_dups = true;
                    println!(
                        "n2: warn: {}: {:?} is repeated in output list",
                        lazy_build.location, f.name,
                    );
                }
                Some(prev) => {
                    anyhow::bail!(
                        "{}: {:?} is already an output at {}",
                        lazy_build.location,
                        f.name,
                        self.lazy_builds[prev].location
                    );
                }
                None => f.input = Some(new_id),
            }
        }
        if fixup_dups {
            lazy_build.deps.outs.remove_duplicates();
        }
        self.lazy_builds.push(lazy_build);
        Ok(())
    }

    pub fn evaluate_build<T: FilenameResolver>(
        &self,
        lazy_build: &LazyBuild,
        resolver: &T,
    ) -> anyhow::Result<graph::Build> {
        let lookup = |key : &str| -> Option<String> {
            lazy_build.lookup(resolver, &self.env, key)
        };

        let cmdline = lookup("command");
        let desc = lookup("description");
        let depfile = lookup("depfile");
        let parse_showincludes = match lookup("deps").as_deref() {
            None => false,
            Some("gcc") => false,
            Some("msvc") => true,
            Some(other) => bail!("invalid deps attribute {:?}", other),
        };
        let pool = lookup("pool");

        let rspfile_path = lookup("rspfile");
        let rspfile_content = lookup("rspfile_content");
        let rspfile = match (rspfile_path, rspfile_content) {
            (None, None) => None,
            (Some(path), Some(content)) => Some(RspFile {
                path: std::path::PathBuf::from(path),
                content,
            }),
            _ => bail!("rspfile and rspfile_content need to be both specified"),
        };
        let hide_success = lookup("hide_success").is_some();
        let hide_progress = lookup("hide_progress").is_some();

        let mut build = graph::Build::new(lazy_build.deps.clone());
        build.cmdline = cmdline;
        build.desc = desc;
        build.depfile = depfile;
        build.parse_showincludes = parse_showincludes;
        build.rspfile = rspfile;
        build.pool = pool;
        build.hide_success = hide_success;
        build.hide_progress = hide_progress;

        Ok(build)
    }

    pub fn evaluate_builds(&mut self) -> anyhow::Result<()> {
        for id in self.lazy_builds.all_ids() {
            let lazy_build = match self.lazy_builds.lookup(id) {
                Some(lb) => lb,
                None => bail!("unknown build id {:?}", id),
            };

            let build = self.evaluate_build(lazy_build, &self.graph.files)?;
            self.graph.builds.push(build);
        }
        Ok(())
    }

    fn read_file(&mut self, id: FileId) -> anyhow::Result<()> {
        let path = self.graph.file(id).path().to_path_buf();
        let bytes = match trace::scope("read file", || scanner::read_file_with_nul(&path)) {
            Ok(b) => b,
            Err(e) => bail!("read {}: {}", path.display(), e),
        };
        self.parse(path, &bytes)
    }

    fn evaluate_and_read_file(
        &mut self,
        file: EvalString<&str>,
        envs: &[&dyn eval::Env],
    ) -> anyhow::Result<()> {
        let evaluated = self.evaluate_path(file, envs);
        self.read_file(evaluated)
    }

    pub fn parse(&mut self, path: PathBuf, bytes: &[u8]) -> anyhow::Result<()> {
        let filename = std::rc::Rc::new(path);

        let mut parser = parse::Parser::new(&bytes);

        loop {
            let stmt = match parser
                .read()
                .map_err(|err| anyhow!(parser.format_parse_error(&filename, err)))?
            {
                None => break,
                Some(s) => s,
            };
            match stmt {
                Statement::Include(id) => trace::scope("include", || {
                    self.evaluate_and_read_file(id, &[&parser.vars])
                })?,
                // TODO: implement scoping for subninja
                Statement::Subninja(id) => trace::scope("subninja", || {
                    self.evaluate_and_read_file(id, &[&parser.vars])
                })?,
                Statement::Default(defaults) => {
                    let evaluated = self.evaluate_paths(defaults, &[&parser.vars]);
                    self.default.extend(evaluated);
                }
                Statement::Rule(rule) => {
                    self.rules.insert(rule.name.to_owned(), rule.vars);
                }
                Statement::Build(build) => self.add_build(filename.clone(), &parser.vars, build)?,
                Statement::Pool(pool) => {
                    self.pools.insert(pool.name.to_string(), pool.depth);
                }
            };
        }
        self.env = parser.vars;
        Ok(())
    }
}

#[derive(Clone)]
pub struct LazyBuild {
    pub deps: BuildDeps,

    pub rule: VarList,

    pub vars: VarList,
}
impl Deref for LazyBuild {
    type Target = BuildDeps;

    fn deref(&self) -> &Self::Target {
        &self.deps
    }
}

impl LazyBuild {
    pub fn new(
        loc: FileLoc,
        ins: BuildIns,
        outs: BuildOuts,
        rule: VarList,
        vars: VarList,
    ) -> Self {
        LazyBuild {
            deps: BuildDeps::new(loc, ins, outs),
            rule,
            vars,
        }
    }

    pub fn lookup(&self, resolver: &dyn FilenameResolver, env: &eval::Vars, key: &str) -> Option<String> {
        let implicit_vars = BuildImplicitVars {
            resolver,
            build: self,
        };

        // Look up `key = ...` binding in build and rule block.
        // See "Variable scope" in the design notes.
        Some(match self.vars.get(key) {
            Some(val) => val.evaluate(&[env]),
            None => self.rule.get(key)?.evaluate(&[&implicit_vars, &self.vars, env]),
        })
    }
}

/// State loaded by read().
pub struct State {
    pub graph: graph::Graph,
    pub db: db::Writer,
    pub hashes: graph::Hashes,
    pub default: Vec<FileId>,
    pub pools: SmallMap<String, usize>,
}

/// Load build.ninja/.n2_db and return the loaded build graph and state.
pub fn read(build_filename: &str) -> anyhow::Result<State> {
    let mut loader = Loader::new();
    trace::scope("loader.read_file", || {
        let id = loader
            .graph
            .files
            .id_from_canonical(to_owned_canon_path(build_filename));
        loader.read_file(id)?;
        loader.evaluate_builds()
    })?;
    let mut hashes = graph::Hashes::default();
    let db = trace::scope("db::open", || {
        let mut db_path = PathBuf::from(".n2_db");
        if let Some(builddir) = &loader.env.get("builddir") {
            db_path = Path::new(&builddir).join(db_path);
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        };
        db::open(&db_path, &mut loader.graph, &mut hashes)
    })
    .map_err(|err| anyhow!("load .n2_db: {}", err))?;
    Ok(State {
        graph: loader.graph,
        db,
        hashes,
        default: loader.default,
        pools: loader.pools,
    })
}

/// Parse a single file's content.
#[cfg(test)]
pub fn parse(name: &str, mut content: Vec<u8>) -> anyhow::Result<graph::Graph> {
    content.push(0);
    let mut loader = Loader::new();
    trace::scope("loader.read_file", || {
        loader.parse(PathBuf::from(name), &content)
    })?;
    Ok(loader.graph)
}

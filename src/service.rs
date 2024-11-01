use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

use crate::{collector::{CollectorSender, CollectorService}, message::Message};
use dashmap::DashMap;
use oxc_allocator::Allocator;
use oxc_parser::{ParseOptions, Parser};
use oxc_resolver::Resolver;
use oxc_semantic::{ModuleRecord, SemanticBuilder};
use oxc_span::{SourceType, VALID_EXTENSIONS};
use rayon::{iter::ParallelBridge, prelude::ParallelIterator};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    loader::{JavaScriptSource, PartialLoader, LINT_PARTIAL_LOADER_EXT},
    utils::read_to_string,
};

pub type Error = miette::Error;

pub struct AnalyzeServiceOptions {
    /// Current working directory
    cwd: Box<Path>,

    /// All paths to lint
    paths: Vec<Box<Path>>,

    /// TypeScript `tsconfig.json` path for reading path alias and project references
    tsconfig: Option<PathBuf>,

    cross_module: bool,
}

impl AnalyzeServiceOptions {
    #[must_use]
    pub fn new<T>(cwd: T, paths: Vec<Box<Path>>) -> Self
    where
        T: Into<Box<Path>>,
    {
        Self {
            cwd: cwd.into(),
            paths,
            tsconfig: None,
            cross_module: false,
        }
    }

    #[inline]
    #[must_use]
    pub fn with_tsconfig<T>(mut self, tsconfig: T) -> Self
    where
        T: Into<PathBuf>,
    {
        let tsconfig = tsconfig.into();
        // Should this be canonicalized?
        let tsconfig = if tsconfig.is_relative() {
            self.cwd.join(tsconfig)
        } else {
            tsconfig
        };
        debug_assert!(tsconfig.is_file());

        self.tsconfig = Some(tsconfig);
        self
    }

    #[inline]
    #[must_use]
    pub fn with_cross_module(mut self, cross_module: bool) -> Self {
        self.cross_module = cross_module;
        self
    }

    #[inline]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

#[derive(Clone)]
pub struct AnalyzeService {
    runtime: Arc<Runtime>,
}

impl AnalyzeService {
    pub fn new(options: AnalyzeServiceOptions) -> Self {
        let runtime = Arc::new(Runtime::new(options));
        Self { runtime }
    }

    pub fn number_of_dependencies(&self) -> usize {
        self.runtime.module_map.len() - self.runtime.paths.len()
    }

    /// # Panics
    pub fn run(&self, tx_error: &CollectorSender) {
        self.runtime
            .paths
            .iter()
            .par_bridge()
            .for_each_with(&self.runtime, |runtime, path| {
                runtime.process_path(path, tx_error)
            });
        tx_error.send(None).unwrap();
    }

    /// For tests
    #[cfg(test)]
    pub(crate) fn run_source<'a>(
        &self,
        allocator: &'a Allocator,
        source_text: &'a str,
        check_syntax_errors: bool,
        tx_error: &CollectorSender,
    ) -> Vec<Message> {
        use crate::collector::CollectorSender;

        self.runtime
            .paths
            .iter()
            .flat_map(|path| {
                let source_type = SourceType::from_path(path).unwrap();
                self.runtime.init_cache_state(path);
                self.runtime.process_source(
                    path,
                    allocator,
                    source_text,
                    source_type,
                    check_syntax_errors,
                    tx_error,
                )
            })
            .collect::<Vec<_>>()
    }
}

/// `CacheState` and `CacheStateEntry` are used to fix the problem where
/// there is a brief moment when a concurrent fetch can miss the cache.
///
/// Given `ModuleMap` is a `DashMap`, which conceptually is a `RwLock<HashMap>`.
/// When two requests read the map at the exact same time from different threads,
/// both will miss the cache so both thread will make a request.
///
/// See the "problem section" in <https://medium.com/@polyglot_factotum/rust-concurrency-patterns-condvars-and-locks-e278f18db74f>
/// and the solution is copied here to fix the issue.
type CacheState = Mutex<FxHashMap<Box<Path>, Arc<(Mutex<CacheStateEntry>, Condvar)>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CacheStateEntry {
    ReadyToConstruct,
    PendingStore(usize),
}

/// Keyed by canonicalized path
type ModuleMap = DashMap<Box<Path>, ModuleState>;

#[derive(Clone)]
enum ModuleState {
    Resolved(Arc<ModuleRecord>),
    Ignored,
}

pub struct Runtime {
    cwd: Box<Path>,
    /// All paths to lint
    paths: FxHashSet<Box<Path>>,
    resolver: Option<Resolver>,
    module_map: ModuleMap,
    cache_state: CacheState,
}

impl Runtime {
    fn new(options: AnalyzeServiceOptions) -> Self {
        let resolver = options.cross_module.then(|| {
            Self::get_resolver(
                options
                    .tsconfig
                    .or_else(|| Some(options.cwd.join("tsconfig.json"))),
            )
        });
        Self {
            cwd: options.cwd,
            paths: options.paths.iter().cloned().collect(),
            resolver,
            module_map: ModuleMap::default(),
            cache_state: CacheState::default(),
        }
    }

    fn get_resolver(tsconfig: Option<PathBuf>) -> Resolver {
        use oxc_resolver::{ResolveOptions, TsconfigOptions, TsconfigReferences};
        let tsconfig = tsconfig.and_then(|path| {
            if path.is_file() {
                Some(TsconfigOptions {
                    config_file: path,
                    references: TsconfigReferences::Auto,
                })
            } else {
                None
            }
        });

        Resolver::new(ResolveOptions {
            extensions: VALID_EXTENSIONS
                .iter()
                .map(|ext| format!(".{ext}"))
                .collect(),
            condition_names: vec!["module".into(), "require".into()],
            tsconfig,
            ..ResolveOptions::default()
        })
    }

    fn get_source_type_and_text(
        path: &Path,
        ext: &str,
    ) -> Option<Result<(SourceType, String), Error>> {
        let source_type = SourceType::from_path(path);
        let not_supported_yet = source_type
            .as_ref()
            .is_err_and(|_| !LINT_PARTIAL_LOADER_EXT.contains(&ext));
        if not_supported_yet {
            return None;
        }
        let source_type = source_type.unwrap_or_default();
        let file_result = read_to_string(path)
            .map_err(|e| Error::msg(format!("Failed to open file {path:?} with error \"{e}\"")));
        Some(match file_result {
            Ok(source_text) => Ok((source_type, source_text)),
            Err(e) => Err(e),
        })
    }

    fn process_path(&self, path: &Path, tx_error: &CollectorSender) {
        if self.init_cache_state(path) {
            return;
        }

        let Some(ext) = path.extension().and_then(OsStr::to_str) else {
            self.ignore_path(path);
            return;
        };

        let Some(source_type_and_text) = Self::get_source_type_and_text(path, ext) else {
            self.ignore_path(path);
            return;
        };

        let (source_type, source_text) = match source_type_and_text {
            Ok(source_text) => source_text,
            Err(e) => {
                self.ignore_path(path);
                tx_error.send(Some((path.to_path_buf(), vec![format!("{}", e)]))).unwrap();
                return;
            }
        };

        let sources = PartialLoader::parse(ext, &source_text);
        let sources = sources
            .unwrap_or_else(|| vec![JavaScriptSource::partial(&source_text, source_type, 0)]);

        if sources.is_empty() {
            self.ignore_path(path);
            return;
        }

        for JavaScriptSource {
            source_text,
            source_type,
            ..
        } in sources
        {
            let allocator = Allocator::default();
            let mut messages =
                self.process_source(path, &allocator, source_text, source_type, true, tx_error);

            if !messages.is_empty() {
                let path = path.strip_prefix(&self.cwd).unwrap_or(path);
                let diagnostics = CollectorService::wrap_messages(path, messages);
                tx_error.send(Some(diagnostics)).unwrap();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn process_source<'a>(
        &self,
        path: &Path,
        allocator: &'a Allocator,
        source_text: &'a str,
        source_type: SourceType,
        check_syntax_errors: bool,
        tx_error: &CollectorSender,
    ) -> Vec<Message> {
        let ret = Parser::new(allocator, source_text, source_type)
            .with_options(ParseOptions {
                parse_regular_expression: true,
                allow_return_outside_function: true,
                ..ParseOptions::default()
            })
            .parse();

        if !ret.errors.is_empty() {
            tx_error.send(Some((path.to_path_buf(), ret.errors.iter().map(|e| format!("{}", e)).collect()))).unwrap();
        };

        let program = allocator.alloc(ret.program);

        let trivias = ret.trivias;

        // Build the module record to unblock other threads from waiting for too long.
        // The semantic model is not built at this stage.
        let semantic_builder = SemanticBuilder::new(source_text)
            .with_cfg(true)
            .with_trivias(trivias)
            .with_build_jsdoc(true)
            .with_check_syntax_error(check_syntax_errors)
            .build_module_record(path, program);
        let module_record = semantic_builder.module_record();

        if self.resolver.is_some() {
            self.module_map.insert(
                path.to_path_buf().into_boxed_path(),
                ModuleState::Resolved(Arc::clone(&module_record)),
            );
            self.update_cache_state(path);

            // Retrieve all dependency modules from this module.
            let dir = path.parent().unwrap();
            module_record
                .requested_modules
                .keys()
                .par_bridge()
                .map_with(self.resolver.as_ref().unwrap(), |resolver, specifier| {
                    resolver
                        .resolve(dir, specifier)
                        .ok()
                        .map(|r| (specifier, r))
                })
                .flatten()
                .for_each_with(tx_error, |tx_error, (specifier, resolution)| {
                    let path = resolution.path();

                    self.process_path(path, tx_error);
                    let Some(target_module_record_ref) = self.module_map.get(path) else {
                        return;
                    };
                    let ModuleState::Resolved(target_module_record) =
                        target_module_record_ref.value()
                    else {
                        return;
                    };
                    // Append target_module to loaded_modules
                    module_record
                        .loaded_modules
                        .insert(specifier.clone(), Arc::clone(target_module_record));
                });

            // The thread is blocked here until all dependent modules are resolved.

            // Resolve and append `star_export_bindings`
            for export_entry in &module_record.star_export_entries {
                let Some(remote_module_record_ref) =
                    export_entry
                        .module_request
                        .as_ref()
                        .and_then(|module_request| {
                            module_record.loaded_modules.get(module_request.name())
                        })
                else {
                    continue;
                };
                let remote_module_record = remote_module_record_ref.value();

                // Append both remote `bindings` and `exported_bindings_from_star_export`
                let remote_exported_bindings_from_star_export = remote_module_record
                    .exported_bindings_from_star_export
                    .iter()
                    .flat_map(|r| r.value().clone());
                let remote_bindings = remote_module_record
                    .exported_bindings
                    .keys()
                    .cloned()
                    .chain(remote_exported_bindings_from_star_export)
                    .collect::<Vec<_>>();
                module_record
                    .exported_bindings_from_star_export
                    .entry(remote_module_record.resolved_absolute_path.clone())
                    .or_default()
                    .value_mut()
                    .extend(remote_bindings);
            }
        }


        let mut import_modules: Vec<Message> = vec![];

        module_record.loaded_modules.iter().for_each(|module| {
            let module_path = String::from(module.resolved_absolute_path.to_str().unwrap_or("unknown"));
            import_modules.push(Message {
                file_path: module_path,
            });
        });

        import_modules
    }

    fn init_cache_state(&self, path: &Path) -> bool {
        if self.resolver.is_none() {
            return false;
        }

        let (lock, cvar) = {
            let mut state_map = self.cache_state.lock().unwrap();
            &*Arc::clone(
                state_map
                    .entry(path.to_path_buf().into_boxed_path())
                    .or_insert_with(|| {
                        Arc::new((
                            Mutex::new(CacheStateEntry::ReadyToConstruct),
                            Condvar::new(),
                        ))
                    }),
            )
        };
        let mut state = cvar
            .wait_while(lock.lock().unwrap(), |state| {
                matches!(*state, CacheStateEntry::PendingStore(_))
            })
            .unwrap();

        let cache_hit = if self.module_map.contains_key(path) {
            true
        } else {
            let i = if let CacheStateEntry::PendingStore(i) = *state {
                i
            } else {
                0
            };
            *state = CacheStateEntry::PendingStore(i + 1);
            false
        };

        if *state == CacheStateEntry::ReadyToConstruct {
            cvar.notify_one();
        }

        drop(state);
        cache_hit
    }

    fn update_cache_state(&self, path: &Path) {
        let (lock, cvar) = {
            let mut state_map = self.cache_state.lock().unwrap();
            &*Arc::clone(
                state_map
                    .get_mut(path)
                    .expect("Entry in http-cache state to have been previously inserted"),
            )
        };
        let mut state = lock.lock().unwrap();
        if let CacheStateEntry::PendingStore(i) = *state {
            let new = i - 1;
            if new == 0 {
                *state = CacheStateEntry::ReadyToConstruct;
                // Notify the next thread waiting in line, if there is any.
                cvar.notify_one();
            } else {
                *state = CacheStateEntry::PendingStore(new);
            }
        }
    }

    fn ignore_path(&self, path: &Path) {
        if self.resolver.is_some() {
            self.module_map
                .insert(path.to_path_buf().into_boxed_path(), ModuleState::Ignored);
            self.update_cache_state(path);
        }
    }
}

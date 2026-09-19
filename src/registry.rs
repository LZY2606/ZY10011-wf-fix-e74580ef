use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::convert::AsRef;
use std::fmt::{self, Debug, Formatter};
use std::io::{Error as IoError, Write};
use std::path::Path;
use std::sync::Arc;

use serde::Serialize;

use crate::context::Context;
use crate::decorators::{self, DecoratorDef};
#[cfg(feature = "script_helper")]
use crate::error::ScriptError;
use crate::error::{RenderError, RenderErrorReason, TemplateError, TemplatePackageError};
use crate::helpers::{self, HelperDef};
use crate::output::{Output, StringOutput, WriteOutput};
use crate::render::{RenderContext, Renderable};
use crate::sources::{FileSource, Source};
use crate::support::str::{self, StringWriter};
use crate::template::{Parameter, Template, TemplateElement, TemplateOptions};

#[cfg(feature = "dir_source")]
use walkdir::WalkDir;

#[cfg(feature = "dir_source")]
use derive_builder::Builder;

#[cfg(feature = "script_helper")]
use rhai::Engine;

#[cfg(feature = "script_helper")]
use crate::helpers::scripting::ScriptHelper;

#[cfg(feature = "rust-embed")]
use crate::sources::LazySource;
#[cfg(feature = "rust-embed")]
use rust_embed::RustEmbed;

/// This type represents an *escape fn*, that is a function whose purpose it is
/// to escape potentially problematic characters in a string.
///
/// An *escape fn* is represented as a `Box` to avoid unnecessary type
/// parameters (and because traits cannot be aliased using `type`).
pub type EscapeFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// The default *escape fn* replaces the characters `&"<>`
/// with the equivalent html / xml entities.
pub fn html_escape(data: &str) -> String {
    str::escape_html(data)
}

/// `EscapeFn` that does not change anything. Useful when using in a non-html
/// environment.
pub fn no_escape(data: &str) -> String {
    data.to_owned()
}

/// The single entry point of your Handlebars templates
///
/// It maintains compiled templates and registered helpers.
#[derive(Clone)]
pub struct Registry<'reg> {
    templates: HashMap<String, Template>,

    helpers: HashMap<String, Arc<dyn HelperDef + Send + Sync + 'reg>>,
    decorators: HashMap<String, Arc<dyn DecoratorDef + Send + Sync + 'reg>>,

    escape_fn: EscapeFn,
    strict_mode: bool,
    dev_mode: bool,
    recursive_lookup: bool,
    prevent_indent: bool,
    /// Names installed by the latest template package update. A package
    /// update replaces exactly this set of names.
    package_templates: BTreeSet<String>,
    #[cfg(feature = "script_helper")]
    pub(crate) engine: Arc<Engine>,

    template_sources:
        HashMap<String, Arc<dyn Source<Item = String, Error = IoError> + Send + Sync + 'reg>>,
    #[cfg(feature = "script_helper")]
    script_sources:
        HashMap<String, Arc<dyn Source<Item = String, Error = IoError> + Send + Sync + 'reg>>,
}

impl Debug for Registry<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("Handlebars")
            .field("templates", &self.templates)
            .field("helpers", &self.helpers.keys())
            .field("decorators", &self.decorators.keys())
            .field("strict_mode", &self.strict_mode)
            .field("dev_mode", &self.dev_mode)
            .finish()
    }
}

impl Default for Registry<'_> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "script_helper")]
fn rhai_engine() -> Engine {
    Engine::new()
}

/// Options for importing template files from a directory.
#[non_exhaustive]
#[derive(Builder)]
#[builder(default)]
#[cfg(feature = "dir_source")]
pub struct DirectorySourceOptions {
    /// The name extension for template files
    #[builder(setter(into))]
    pub tpl_extension: String,
    /// Whether to include hidden files (file name that starts with `.`)
    pub hidden: bool,
    /// Whether to include temporary files (file name that starts with `#`)
    pub temporary: bool,
}

#[cfg(feature = "dir_source")]
impl DirectorySourceOptions {
    fn ignore_file(&self, name: &str) -> bool {
        self.ignored_as_hidden_file(name) || self.ignored_as_temporary_file(name)
    }

    #[inline]
    fn ignored_as_hidden_file(&self, name: &str) -> bool {
        !self.hidden && name.starts_with('.')
    }

    #[inline]
    fn ignored_as_temporary_file(&self, name: &str) -> bool {
        !self.temporary && name.starts_with('#')
    }
}

#[cfg(feature = "dir_source")]
impl Default for DirectorySourceOptions {
    fn default() -> Self {
        DirectorySourceOptions {
            tpl_extension: ".hbs".to_owned(),
            hidden: false,
            temporary: false,
        }
    }
}

impl<'reg> Registry<'reg> {
    pub fn new() -> Registry<'reg> {
        let r = Registry {
            templates: HashMap::new(),
            template_sources: HashMap::new(),
            helpers: HashMap::new(),
            decorators: HashMap::new(),
            escape_fn: Arc::new(html_escape),
            strict_mode: false,
            dev_mode: false,
            recursive_lookup: false,
            prevent_indent: false,
            package_templates: BTreeSet::new(),
            #[cfg(feature = "script_helper")]
            engine: Arc::new(rhai_engine()),
            #[cfg(feature = "script_helper")]
            script_sources: HashMap::new(),
        };

        r.setup_builtins()
    }

    fn setup_builtins(mut self) -> Registry<'reg> {
        self.register_helper("if", Box::new(helpers::IF_HELPER));
        self.register_helper("unless", Box::new(helpers::UNLESS_HELPER));
        self.register_helper("each", Box::new(helpers::EACH_HELPER));
        self.register_helper("with", Box::new(helpers::WITH_HELPER));
        self.register_helper("lookup", Box::new(helpers::LOOKUP_HELPER));
        self.register_helper("raw", Box::new(helpers::RAW_HELPER));
        self.register_helper("log", Box::new(helpers::LOG_HELPER));

        self.register_helper("eq", Box::new(helpers::helper_extras::EQ_HELPER));
        self.register_helper("ne", Box::new(helpers::helper_extras::NEQ_HELPER));
        self.register_helper("gt", Box::new(helpers::helper_extras::GT_HELPER));
        self.register_helper("gte", Box::new(helpers::helper_extras::GTE_HELPER));
        self.register_helper("lt", Box::new(helpers::helper_extras::LT_HELPER));
        self.register_helper("lte", Box::new(helpers::helper_extras::LTE_HELPER));
        self.register_helper("and", Box::new(helpers::helper_extras::AND_HELPER));
        self.register_helper("or", Box::new(helpers::helper_extras::OR_HELPER));
        self.register_helper("not", Box::new(helpers::helper_extras::NOT_HELPER));
        self.register_helper("len", Box::new(helpers::helper_extras::len));

        #[cfg(feature = "string_helpers")]
        self.register_string_helpers();

        self.register_decorator("inline", Box::new(decorators::INLINE_DECORATOR));
        self
    }

    /// Enable or disable recursive variable resolution mode
    ///
    /// By default variable resolution is performed directly
    /// within the current scope.
    ///
    /// For certain legacy use cases it may be desirable for variable
    /// resolution to walk up through the enclosing scopes until
    /// a matching variable is found.
    pub fn set_recursive_lookup(&mut self, enabled: bool) {
        self.recursive_lookup = enabled;
    }

    /// Return recursive lookup state, default is false.
    pub fn recursive_lookup(&self) -> bool {
        self.recursive_lookup
    }

    /// Enable or disable handlebars strict mode
    ///
    /// By default, handlebars renders empty string for value that
    /// undefined or never exists. Since rust is a static type
    /// language, we offer strict mode in handlebars-rust.  In strict
    /// mode, if you were to render a value that doesn't exist, a
    /// `RenderError` will be raised.
    pub fn set_strict_mode(&mut self, enabled: bool) {
        self.strict_mode = enabled;
    }

    /// Return strict mode state, default is false.
    ///
    /// By default, handlebars renders empty string for value that
    /// undefined or never exists. Since rust is a static type
    /// language, we offer strict mode in handlebars-rust.  In strict
    /// mode, if you were access a value that doesn't exist, a
    /// `RenderError` will be raised.
    pub fn strict_mode(&self) -> bool {
        self.strict_mode
    }

    /// Return dev mode state, default is false
    ///
    /// With dev mode turned on, handlebars enables a set of development
    /// friendly features, that may affect its performance.
    pub fn dev_mode(&self) -> bool {
        self.dev_mode
    }

    /// Enable or disable dev mode
    ///
    /// With dev mode turned on, handlebars enables a set of development
    /// friendly features, that may affect its performance.
    ///
    /// **Note that you have to enable dev mode before adding templates to
    /// the registry**. Otherwise it won't take effect at all.
    pub fn set_dev_mode(&mut self, enabled: bool) {
        self.dev_mode = enabled;

        // clear template source when disabling dev mode
        if !enabled {
            self.template_sources.clear();
        }
    }

    /// Enable or disable indent for partial include tag `{{>}}`
    ///
    /// By default handlebars keeps indent whitespaces for partial
    /// include tag, to change this behaviour, set this toggle to `true`.
    pub fn set_prevent_indent(&mut self, enable: bool) {
        self.prevent_indent = enable;
    }

    /// Return state for `prevent_indent` option, default to `false`.
    pub fn prevent_indent(&self) -> bool {
        self.prevent_indent
    }

    /// Register a `Template`
    ///
    /// This is infallible since the template has already been parsed and
    /// insert cannot fail. If there is an existing template with this name it
    /// will be overwritten.
    ///
    /// Dev mode doesn't apply for pre-compiled template because it's lifecycle
    /// is not managed by the registry.
    pub fn register_template(&mut self, name: &str, tpl: Template) {
        self.templates.insert(name.to_string(), tpl);
    }

    /// Register a template string
    ///
    /// Returns `TemplateError` if there is syntax error on parsing the template.
    pub fn register_template_string<S>(
        &mut self,
        name: &str,
        tpl_str: S,
    ) -> Result<(), TemplateError>
    where
        S: AsRef<str>,
    {
        let template = Template::compile2(
            tpl_str.as_ref(),
            TemplateOptions {
                name: Some(name.to_owned()),
                is_partial: false,
                prevent_indent: self.prevent_indent,
            },
        )?;
        self.register_template(name, template);
        Ok(())
    }

    /// Register a partial string
    ///
    /// A named partial will be added to the registry. It will overwrite template with
    /// same name. Currently a registered partial is just identical to a template.
    pub fn register_partial<S>(&mut self, name: &str, partial_str: S) -> Result<(), TemplateError>
    where
        S: AsRef<str>,
    {
        self.register_template_string(name, partial_str)
    }

    /// Atomically replace the registry's template package.
    ///
    /// A *template package* is a group of named templates and partials that
    /// are deployed together and may reference each other via `{{> name}}`.
    /// This method installs the given package as a whole:
    ///
    /// * Every submitted template is compiled first. If any of them fails,
    ///   a [`TemplatePackageError`] listing **all** failed members (sorted by
    ///   name) is returned and nothing changes.
    /// * Statically resolvable partial references are checked against the
    ///   state the registry would have after the update: every referenced
    ///   name must be either a member of the submitted package or a template
    ///   registered outside the package. Otherwise an
    ///   [`TemplatePackageError::UnresolvedPartials`] listing every missing
    ///   reference is returned and nothing changes. References that are only
    ///   computable at render time (for example `{{> (lookup . "which")}}`)
    ///   cannot be checked statically and are ignored.
    /// * Only when the whole package compiles and its dependencies are
    ///   closed is it committed: templates installed by the *previous*
    ///   package update but absent from this one are removed (including
    ///   their dev-mode sources, so dev mode cannot resurrect them from
    ///   stale files), and the new members are installed.
    ///
    /// Templates registered through the other `register_*` methods are not
    /// affected, unless their names collide with package members.
    ///
    /// Submitting the same package twice is idempotent: the second call
    /// succeeds and leaves the visible state unchanged.
    ///
    /// # Atomicity
    ///
    /// All fallible work (compilation and dependency checks) happens before
    /// any registry state is touched. The commit point is a short,
    /// infallible sequence of map replacements performed while this method
    /// holds `&mut self`, so no render can observe a mixture of old and new
    /// package members: renders either completed before the update and saw
    /// the old package, or start after it and see the new one. On failure
    /// the registry is left byte-for-byte as it was before the call, so
    /// there is never a half-installed package visible.
    ///
    /// ```
    /// use handlebars::Handlebars;
    ///
    /// let mut hbs = Handlebars::new();
    /// hbs.register_template_package([
    ///     ("layout", "<body>{{> content}}</body>"),
    ///     ("content", "hello {{who}}"),
    /// ])
    /// .unwrap();
    /// assert_eq!(
    ///     hbs.render("layout", &serde_json::json!({"who": "world"}))
    ///         .unwrap(),
    ///     "<body>hello world</body>"
    /// );
    ///
    /// // A broken package changes nothing: the old version keeps working.
    /// assert!(
    ///     hbs.register_template_package([("layout", "{{#if}}{{/each}}")])
    ///         .is_err()
    /// );
    /// assert_eq!(
    ///     hbs.render("layout", &serde_json::json!({"who": "world"}))
    ///         .unwrap(),
    ///     "<body>hello world</body>"
    /// );
    ///
    /// // A package whose partial references dangle is rejected, too.
    /// assert!(
    ///     hbs.register_template_package([("page", "{{> missing}}")])
    ///         .is_err()
    /// );
    /// ```
    pub fn register_template_package<N, S, I>(
        &mut self,
        templates: I,
    ) -> Result<(), TemplatePackageError>
    where
        N: AsRef<str>,
        S: AsRef<str>,
        I: IntoIterator<Item = (N, S)>,
    {
        let mut compiled = BTreeMap::new();
        let mut errors = Vec::new();
        for (name, tpl_str) in templates {
            let name = name.as_ref();
            let result = Template::compile2(
                tpl_str.as_ref(),
                TemplateOptions {
                    name: Some(name.to_owned()),
                    is_partial: false,
                    prevent_indent: self.prevent_indent,
                },
            );
            match result {
                Ok(template) => {
                    compiled.insert(name.to_owned(), (template, None));
                }
                Err(err) => errors.push(err),
            }
        }
        self.commit_template_package(compiled, errors)
    }

    /// Atomically replace the registry's template package with templates
    /// loaded from files.
    ///
    /// Behaves exactly like [`Registry::register_template_package`], except
    /// that template sources are read from the given paths. When dev mode is
    /// enabled, the files are tracked as dev-mode sources (like
    /// [`Registry::register_template_file`]) and reloaded on every render;
    /// members dropped by a later package update have their sources removed,
    /// so dev mode will not load them from the old files again.
    ///
    /// ```
    /// use handlebars::Handlebars;
    /// use std::io::Write;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let path = dir.path().join("greeting.hbs");
    /// let mut file = std::fs::File::create(&path).unwrap();
    /// write!(file, "hi {{{{who}}}}").unwrap();
    /// drop(file);
    ///
    /// let mut hbs = Handlebars::new();
    /// hbs.register_template_package_files([("greeting", &path)])
    ///     .unwrap();
    /// assert_eq!(
    ///     hbs.render("greeting", &serde_json::json!({"who": "world"}))
    ///         .unwrap(),
    ///     "hi world"
    /// );
    /// ```
    pub fn register_template_package_files<N, P, I>(
        &mut self,
        templates: I,
    ) -> Result<(), TemplatePackageError>
    where
        N: AsRef<str>,
        P: AsRef<Path>,
        I: IntoIterator<Item = (N, P)>,
    {
        let mut compiled = BTreeMap::new();
        let mut errors = Vec::new();
        for (name, tpl_path) in templates {
            let name = name.as_ref();
            let source = FileSource::new(tpl_path.as_ref().into());
            let result = source
                .load()
                .map_err(|err| TemplateError::from((err, name.to_owned())))
                .and_then(|tpl_str| {
                    Template::compile2(
                        tpl_str.as_ref(),
                        TemplateOptions {
                            name: Some(name.to_owned()),
                            is_partial: false,
                            prevent_indent: self.prevent_indent,
                        },
                    )
                });
            match result {
                Ok(template) => {
                    let source: Arc<dyn Source<Item = String, Error = IoError> + Send + Sync> =
                        Arc::new(source);
                    compiled.insert(name.to_owned(), (template, Some(source)));
                }
                Err(err) => errors.push(err),
            }
        }
        self.commit_template_package(compiled, errors)
    }

    /// Commit point of a template package update.
    ///
    /// `compiled` holds every package member that compiled successfully and
    /// `errors` every compilation failure. When this function returns an
    /// error, no registry state has been modified; when it returns `Ok`, the
    /// whole package has been installed and the previous package removed.
    fn commit_template_package(
        &mut self,
        compiled: BTreeMap<
            String,
            (
                Template,
                Option<Arc<dyn Source<Item = String, Error = IoError> + Send + Sync + 'reg>>,
            ),
        >,
        mut errors: Vec<TemplateError>,
    ) -> Result<(), TemplatePackageError> {
        if !errors.is_empty() {
            // Report every failed member in a stable order, independent of
            // the caller's iteration order.
            errors.sort_by(|a, b| a.name().cmp(&b.name()));
            return Err(TemplatePackageError::Compile(errors));
        }

        // Check dependency closure against the post-update view: package
        // members plus everything registered outside the package.
        let visible_after_update = |name: &str| {
            compiled.contains_key(name)
                || (self.templates.contains_key(name) && !self.package_templates.contains(name))
        };
        let mut unresolved = Vec::new();
        for (member, (template, _)) in &compiled {
            let mut refs = BTreeSet::new();
            let mut inlines = BTreeSet::new();
            collect_partial_refs(template, &mut refs, &mut inlines);
            for reference in refs.difference(&inlines) {
                if !visible_after_update(reference) {
                    unresolved.push((member.clone(), reference.clone()));
                }
            }
        }
        if !unresolved.is_empty() {
            return Err(TemplatePackageError::UnresolvedPartials(unresolved));
        }

        // From here on nothing can fail: swap the package in one infallible
        // sequence while holding `&mut self`, so concurrent renders (which
        // would need `&self`) cannot observe a half-updated package.
        for old_name in std::mem::take(&mut self.package_templates) {
            self.templates.remove(&old_name);
            self.template_sources.remove(&old_name);
        }
        for (name, (template, source)) in compiled {
            self.templates.insert(name.clone(), template);
            match source {
                Some(source) if self.dev_mode => {
                    self.template_sources.insert(name.clone(), source);
                }
                _ => {
                    // A string-backed package member must not keep a stale
                    // dev-mode source registered under the same name.
                    self.template_sources.remove(&name);
                }
            }
            self.package_templates.insert(name);
        }
        Ok(())
    }

    /// Register a template from a path on file system
    ///
    /// If dev mode is enabled, the registry will keep reading the template file
    /// from file system everytime it's visited.
    pub fn register_template_file<P>(
        &mut self,
        name: &str,
        tpl_path: P,
    ) -> Result<(), TemplateError>
    where
        P: AsRef<Path>,
    {
        let source = FileSource::new(tpl_path.as_ref().into());
        let template_string = source
            .load()
            .map_err(|err| TemplateError::from((err, name.to_owned())))?;

        self.register_template_string(name, template_string)?;
        if self.dev_mode {
            self.template_sources
                .insert(name.to_owned(), Arc::new(source));
        }

        Ok(())
    }

    /// Register templates from a directory
    ///
    /// Hidden files and tempfile (starts with `#`) will be ignored by default.
    /// Set `DirectorySourceOptions` to something other than `DirectorySourceOptions::default()` to adjust this.
    /// All registered templates will use their relative path to determine their template name.
    /// For example, when `dir_path` is `templates/` and `DirectorySourceOptions.tpl_extension` is `.hbs`, the file
    /// `templates/some/path/file.hbs` will be registered as `some/path/file`.
    ///
    /// This method is not available by default.
    /// You will need to enable the `dir_source` feature to use it.
    ///
    /// When dev_mode is enabled, like with `register_template_file`, templates are reloaded
    /// from the file system every time they're visited.
    ///
    /// ```rust
    /// use handlebars::{Handlebars, DirectorySourceOptionsBuilder};
    ///
    /// let mut hbs = Handlebars::new();
    /// hbs.register_templates_directory(
    ///     "/path/to/templates",
    ///     DirectorySourceOptionsBuilder::default()
    ///         .tpl_extension("hbs")
    ///         .build()
    ///         .unwrap(),
    /// ).unwrap();
    /// ```
    #[cfg(feature = "dir_source")]
    #[cfg_attr(docsrs, doc(cfg(feature = "dir_source")))]
    pub fn register_templates_directory<P>(
        &mut self,
        dir_path: P,
        options: DirectorySourceOptions,
    ) -> Result<(), TemplateError>
    where
        P: AsRef<Path>,
    {
        let dir_path = dir_path.as_ref();

        let walker = WalkDir::new(dir_path);
        let dir_iter = walker
            .min_depth(1)
            .into_iter()
            .filter_map(|e| e.ok().map(|e| e.into_path()))
            // Checks if extension matches
            .filter(|tpl_path| {
                tpl_path
                    .to_string_lossy()
                    .ends_with(options.tpl_extension.as_str())
            })
            // Rejects any hidden or temporary files.
            .filter(|tpl_path| {
                tpl_path
                    .file_stem()
                    .map(|stem| !options.ignore_file(&stem.to_string_lossy()))
                    .unwrap_or(false)
            })
            .filter_map(|tpl_path| {
                tpl_path
                    .strip_prefix(dir_path)
                    .ok()
                    .map(|tpl_canonical_name| {
                        let tpl_name = tpl_canonical_name
                            .components()
                            .map(|component| component.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/");

                        tpl_name
                            .strip_suffix(options.tpl_extension.as_str())
                            .map(|s| s.to_owned())
                            .unwrap_or(tpl_name)
                    })
                    .map(|tpl_canonical_name| (tpl_canonical_name, tpl_path))
            });

        for (tpl_canonical_name, tpl_path) in dir_iter {
            self.register_template_file(&tpl_canonical_name, &tpl_path)?;
        }

        Ok(())
    }

    /// Register templates using a
    /// [RustEmbed](https://github.com/pyros2097/rust-embed) type
    /// Calls register_embed_templates_with_extension with empty extension.
    ///
    /// File names from embed struct are used as template name.
    ///
    /// ```skip
    /// #[derive(RustEmbed)]
    /// #[folder = "templates"]
    /// #[include = "*.hbs"]
    /// struct Assets;
    ///
    /// let mut hbs = Handlebars::new();
    /// hbs.register_embed_templates::<Assets>();
    /// ```
    ///
    #[cfg(feature = "rust-embed")]
    #[cfg_attr(docsrs, doc(cfg(feature = "rust-embed")))]
    pub fn register_embed_templates<E>(&mut self) -> Result<(), TemplateError>
    where
        E: RustEmbed,
    {
        self.register_embed_templates_with_extension::<E>("")
    }

    /// Register templates using a
    /// [RustEmbed](https://github.com/pyros2097/rust-embed) type
    /// * `tpl_extension`: the template file extension
    ///
    /// File names from embed struct are used as template name, but extension is stripped.
    ///
    /// When dev_mode enabled templates is reloaded
    /// from embed struct everytime it's visied.
    ///
    /// ```skip
    /// #[derive(RustEmbed)]
    /// #[folder = "templates"]
    /// struct Assets;
    ///
    /// let mut hbs = Handlebars::new();
    /// hbs.register_embed_templates_with_extension::<Assets>(".hbs");
    /// ```
    ///
    #[cfg(feature = "rust-embed")]
    #[cfg_attr(docsrs, doc(cfg(feature = "rust-embed")))]
    pub fn register_embed_templates_with_extension<E>(
        &mut self,
        tpl_extension: &str,
    ) -> Result<(), TemplateError>
    where
        E: RustEmbed,
    {
        for file_name in E::iter().filter(|x| x.ends_with(tpl_extension)) {
            let tpl_name = file_name
                .strip_suffix(tpl_extension)
                .unwrap_or(&file_name)
                .to_owned();
            let source = LazySource::new(move || {
                E::get(&file_name)
                    .map(|file| file.data.to_vec())
                    .and_then(|data| String::from_utf8(data).ok())
            });
            let tpl_content = source
                .load()
                .map_err(|e| (e, "Template load error".to_owned()))?;
            self.register_template_string(&tpl_name, &tpl_content)?;

            if self.dev_mode {
                self.template_sources.insert(tpl_name, Arc::new(source));
            }
        }
        Ok(())
    }

    /// Remove a template from the registry
    pub fn unregister_template(&mut self, name: &str) {
        self.templates.remove(name);
        self.template_sources.remove(name);
        self.package_templates.remove(name);
    }

    /// Register a helper
    pub fn register_helper(&mut self, name: &str, def: Box<dyn HelperDef + Send + Sync + 'reg>) {
        self.helpers.insert(name.to_string(), def.into());
    }

    /// Unregister a helper
    pub fn unregister_helper(&mut self, name: &str) {
        self.helpers.remove(name);
    }

    /// Register a [rhai](https://docs.rs/rhai/) script as handlebars helper
    ///
    /// Currently only simple helpers are supported. You can do computation or
    /// string formatting with rhai script.
    ///
    /// Helper parameters and hash are available in rhai script as array `params`
    /// and map `hash`. Example script:
    ///
    /// ```handlebars
    /// {{percent 0.34 label="%"}}
    /// ```
    ///
    /// ```rhai
    /// // percent.rhai
    /// let value = params[0];
    /// let label = hash["label"];
    ///
    /// (value * 100).to_string() + label
    /// ```
    ///
    #[cfg(feature = "script_helper")]
    #[cfg_attr(docsrs, doc(cfg(feature = "script_helper")))]
    pub fn register_script_helper(&mut self, name: &str, script: &str) -> Result<(), ScriptError> {
        let compiled = self.engine.compile(script)?;
        let script_helper = ScriptHelper { script: compiled };
        self.helpers
            .insert(name.to_string(), Arc::new(script_helper));
        Ok(())
    }

    /// Register a [rhai](https://docs.rs/rhai/) script from file
    ///
    /// When dev mode is enable, script file is reloaded from original file
    /// everytime it is called.
    #[cfg(feature = "script_helper")]
    #[cfg_attr(docsrs, doc(cfg(feature = "script_helper")))]
    pub fn register_script_helper_file<P>(
        &mut self,
        name: &str,
        script_path: P,
    ) -> Result<(), ScriptError>
    where
        P: AsRef<Path>,
    {
        let source = FileSource::new(script_path.as_ref().into());
        let script = source.load()?;

        self.script_sources
            .insert(name.to_owned(), Arc::new(source));
        self.register_script_helper(name, &script)
    }

    /// Borrow a read-only reference to current rhai engine
    #[cfg(feature = "script_helper")]
    #[cfg_attr(docsrs, doc(cfg(feature = "script_helper")))]
    pub fn engine(&self) -> &Engine {
        self.engine.as_ref()
    }

    /// Set a custom rhai engine for the registry.
    ///
    /// *Note that* you need to set custom engine before adding scripts.
    #[cfg(feature = "script_helper")]
    #[cfg_attr(docsrs, doc(cfg(feature = "script_helper")))]
    pub fn set_engine(&mut self, engine: Engine) {
        self.engine = Arc::new(engine);
    }

    /// Register a decorator
    pub fn register_decorator(
        &mut self,
        name: &str,
        def: Box<dyn DecoratorDef + Send + Sync + 'reg>,
    ) {
        self.decorators.insert(name.to_string(), def.into());
    }

    /// Register a new *escape fn* to be used from now on by this registry.
    pub fn register_escape_fn<F: 'static + Fn(&str) -> String + Send + Sync>(
        &mut self,
        escape_fn: F,
    ) {
        self.escape_fn = Arc::new(escape_fn);
    }

    /// Restore the default *escape fn*.
    pub fn unregister_escape_fn(&mut self) {
        self.escape_fn = Arc::new(html_escape);
    }

    /// Get a reference to the current *escape fn*.
    pub fn get_escape_fn(&self) -> &dyn Fn(&str) -> String {
        self.escape_fn.as_ref()
    }

    /// Return `true` if a template is registered for the given name
    pub fn has_template(&self, name: &str) -> bool {
        self.get_template(name).is_some()
    }

    /// Return a registered template,
    pub fn get_template(&self, name: &str) -> Option<&Template> {
        self.templates.get(name)
    }

    #[inline]
    pub(crate) fn get_or_load_template_optional(
        &'reg self,
        name: &str,
    ) -> Option<Result<Cow<'reg, Template>, RenderError>> {
        if let (true, Some(source)) = (self.dev_mode, self.template_sources.get(name)) {
            let r = source
                .load()
                .map_err(|e| TemplateError::from((e, name.to_owned())))
                .and_then(|tpl_str| {
                    Template::compile2(
                        tpl_str.as_ref(),
                        TemplateOptions {
                            name: Some(name.to_owned()),
                            prevent_indent: self.prevent_indent,
                            is_partial: false,
                        },
                    )
                })
                .map(Cow::Owned)
                .map_err(RenderError::from);
            Some(r)
        } else {
            self.templates.get(name).map(|t| Ok(Cow::Borrowed(t)))
        }
    }

    #[inline]
    pub(crate) fn get_or_load_template(
        &'reg self,
        name: &str,
    ) -> Result<Cow<'reg, Template>, RenderError> {
        if let Some(result) = self.get_or_load_template_optional(name) {
            result
        } else {
            Err(RenderErrorReason::TemplateNotFound(name.to_owned()).into())
        }
    }

    /// Return a registered helper
    #[inline]
    pub(crate) fn get_or_load_helper(
        &'reg self,
        name: &str,
    ) -> Result<Option<Arc<dyn HelperDef + Send + Sync + 'reg>>, RenderError> {
        #[cfg(feature = "script_helper")]
        if let (true, Some(source)) = (self.dev_mode, self.script_sources.get(name)) {
            return source
                .load()
                .map_err(ScriptError::from)
                .and_then(|s| {
                    let helper = Box::new(ScriptHelper {
                        script: self.engine.compile(s)?,
                    }) as Box<dyn HelperDef + Send + Sync>;
                    Ok(Some(helper.into()))
                })
                .map_err(|e| RenderError::from(RenderErrorReason::from(e)));
        }

        Ok(self.helpers.get(name).cloned())
    }

    #[inline]
    pub(crate) fn has_helper(&self, name: &str) -> bool {
        self.helpers.contains_key(name)
    }

    /// Return a registered decorator
    #[inline]
    pub(crate) fn get_decorator(
        &self,
        name: &str,
    ) -> Option<&(dyn DecoratorDef + Send + Sync + 'reg)> {
        self.decorators.get(name).map(AsRef::as_ref)
    }

    /// Return all templates registered
    ///
    /// **Note that** in dev mode, the template returned from this method may
    /// not reflect its latest state. This method doesn't try to reload templates
    /// from its source.
    pub fn get_templates(&self) -> &HashMap<String, Template> {
        &self.templates
    }

    /// Unregister all templates
    pub fn clear_templates(&mut self) {
        self.templates.clear();
        self.template_sources.clear();
        self.package_templates.clear();
    }

    fn gather_dev_mode_templates(
        &'reg self,
        prebound: Option<(&str, Cow<'reg, Template>)>,
    ) -> Result<BTreeMap<String, Cow<'reg, Template>>, RenderError> {
        let prebound_name = prebound.as_ref().map(|(name, _)| *name);
        let mut res = BTreeMap::new();
        for name in self.template_sources.keys() {
            if Some(&**name) == prebound_name {
                continue;
            }
            res.insert(name.clone(), self.get_or_load_template(name)?);
        }
        if let Some((name, prebound)) = prebound {
            res.insert(name.to_owned(), prebound);
        }
        Ok(res)
    }

    fn render_resolved_template_to_output(
        &self,
        name: Option<&str>,
        template: Cow<'_, Template>,
        ctx: &Context,
        output: &mut impl Output,
    ) -> Result<(), RenderError> {
        if !self.dev_mode {
            let mut render_context = RenderContext::new(template.name.as_ref());
            render_context.set_recursive_lookup(self.recursive_lookup);
            return template.render(self, ctx, &mut render_context, output);
        }

        let dev_mode_templates;
        let template = if let Some(name) = name {
            dev_mode_templates = self.gather_dev_mode_templates(Some((name, template)))?;
            &dev_mode_templates[name]
        } else {
            dev_mode_templates = self.gather_dev_mode_templates(None)?;
            &template
        };

        let mut render_context = RenderContext::new(template.name.as_ref());

        render_context.set_dev_mode_templates(Some(&dev_mode_templates));
        render_context.set_recursive_lookup(self.recursive_lookup);

        template.render(self, ctx, &mut render_context, output)
    }

    #[inline]
    fn render_to_output<O>(
        &self,
        name: &str,
        ctx: &Context,
        output: &mut O,
    ) -> Result<(), RenderError>
    where
        O: Output,
    {
        self.render_resolved_template_to_output(
            Some(name),
            self.get_or_load_template(name)?,
            ctx,
            output,
        )
    }

    /// Render a registered template with some data into a string
    ///
    /// * `name` is the template name you registered previously
    /// * `data` is the data that implements `serde::Serialize`
    ///
    /// Returns rendered string or a struct with error information
    pub fn render<T>(&self, name: &str, data: &T) -> Result<String, RenderError>
    where
        T: Serialize,
    {
        let mut output = StringOutput::new();
        let ctx = Context::wraps(data)?;
        self.render_to_output(name, &ctx, &mut output)?;
        output.into_string().map_err(RenderError::from)
    }

    /// Render a registered template with reused context
    pub fn render_with_context(&self, name: &str, ctx: &Context) -> Result<String, RenderError> {
        let mut output = StringOutput::new();
        self.render_to_output(name, ctx, &mut output)?;
        output.into_string().map_err(RenderError::from)
    }

    /// Render a registered template and write data to the `std::io::Write`
    pub fn render_to_write<T, W>(&self, name: &str, data: &T, writer: W) -> Result<(), RenderError>
    where
        T: Serialize,
        W: Write,
    {
        let mut output = WriteOutput::new(writer);
        let ctx = Context::wraps(data)?;
        self.render_to_output(name, &ctx, &mut output)
    }

    /// Render a registered template using reusable `Context`, and write data to
    /// the `std::io::Write`
    pub fn render_with_context_to_write<W>(
        &self,
        name: &str,
        ctx: &Context,
        writer: W,
    ) -> Result<(), RenderError>
    where
        W: Write,
    {
        let mut output = WriteOutput::new(writer);
        self.render_to_output(name, ctx, &mut output)
    }

    /// Render a template string using current registry without registering it
    pub fn render_template<T>(&self, template_string: &str, data: &T) -> Result<String, RenderError>
    where
        T: Serialize,
    {
        let mut writer = StringWriter::new();
        self.render_template_to_write(template_string, data, &mut writer)?;
        Ok(writer.into_string())
    }

    /// Render a template string using reusable context data
    pub fn render_template_with_context(
        &self,
        template_string: &str,
        ctx: &Context,
    ) -> Result<String, RenderError> {
        let tpl = Template::compile2(
            template_string,
            TemplateOptions {
                prevent_indent: self.prevent_indent,
                ..Default::default()
            },
        )
        .map_err(RenderError::from)?;

        let mut out = StringOutput::new();
        self.render_resolved_template_to_output(None, Cow::Owned(tpl), ctx, &mut out)?;

        out.into_string().map_err(RenderError::from)
    }

    /// Render a template string using resuable context, and write data into
    /// `std::io::Write`
    pub fn render_template_with_context_to_write<W>(
        &self,
        template_string: &str,
        ctx: &Context,
        writer: W,
    ) -> Result<(), RenderError>
    where
        W: Write,
    {
        let tpl = Template::compile2(
            template_string,
            TemplateOptions {
                prevent_indent: self.prevent_indent,
                ..Default::default()
            },
        )
        .map_err(RenderError::from)?;
        let mut out = WriteOutput::new(writer);

        self.render_resolved_template_to_output(None, Cow::Owned(tpl), ctx, &mut out)
    }

    /// Render a template string using current registry without registering it
    pub fn render_template_to_write<T, W>(
        &self,
        template_string: &str,
        data: &T,
        writer: W,
    ) -> Result<(), RenderError>
    where
        T: Serialize,
        W: Write,
    {
        let ctx = Context::wraps(data)?;
        self.render_template_with_context_to_write(template_string, &ctx, writer)
    }

    #[cfg(feature = "string_helpers")]
    #[inline]
    fn register_string_helpers(&mut self) {
        use helpers::string_helpers::{
            kebab_case, lower_camel_case, shouty_kebab_case, shouty_snake_case, snake_case,
            title_case, train_case, upper_camel_case,
        };

        self.register_helper("lowerCamelCase", Box::new(lower_camel_case));
        self.register_helper("upperCamelCase", Box::new(upper_camel_case));
        self.register_helper("snakeCase", Box::new(snake_case));
        self.register_helper("kebabCase", Box::new(kebab_case));
        self.register_helper("shoutySnakeCase", Box::new(shouty_snake_case));
        self.register_helper("shoutyKebabCase", Box::new(shouty_kebab_case));
        self.register_helper("titleCase", Box::new(title_case));
        self.register_helper("trainCase", Box::new(train_case));
    }
}

/// Extract a statically known partial name from a parameter, if any.
///
/// Dynamic names (subexpressions like `{{> (lookup . "which")}}`) cannot be
/// resolved without render-time data and return `None`.
fn static_partial_name(param: &Parameter) -> Option<String> {
    match param {
        Parameter::Name(name) => Some(name.clone()),
        Parameter::Path(path) => Some(path.raw().to_owned()),
        Parameter::Literal(serde_json::Value::String(name)) => Some(name.clone()),
        _ => None,
    }
}

/// Collect statically resolvable partial references of a template into
/// `refs`, and names of inline partials defined within the same template
/// into `inlines`.
///
/// Inline partials (`{{#*inline "name"}}`) are render-context locals, so a
/// reference to one of them inside the defining template does not require a
/// registry-level template of that name.
fn collect_partial_refs(
    template: &Template,
    refs: &mut BTreeSet<String>,
    inlines: &mut BTreeSet<String>,
) {
    for element in &template.elements {
        match element {
            TemplateElement::PartialExpression(d) | TemplateElement::PartialBlock(d) => {
                if let Some(name) = static_partial_name(&d.name) {
                    if name != crate::partial::PARTIAL_BLOCK {
                        refs.insert(name);
                    }
                }
                if let Some(inner) = &d.template {
                    collect_partial_refs(inner, refs, inlines);
                }
            }
            TemplateElement::DecoratorExpression(d) | TemplateElement::DecoratorBlock(d) => {
                if matches!(&d.name, Parameter::Name(name) if name == "inline") {
                    if let Some(Parameter::Literal(serde_json::Value::String(name))) =
                        d.params.first()
                    {
                        inlines.insert(name.clone());
                    }
                }
                if let Some(inner) = &d.template {
                    collect_partial_refs(inner, refs, inlines);
                }
            }
            TemplateElement::HelperBlock(h) => {
                if let Some(inner) = &h.template {
                    collect_partial_refs(inner, refs, inlines);
                }
                if let Some(inner) = &h.inverse {
                    collect_partial_refs(inner, refs, inlines);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod test {
    use crate::context::Context;
    use crate::error::{RenderError, RenderErrorReason};
    use crate::helpers::HelperDef;
    use crate::output::Output;
    use crate::registry::Registry;
    use crate::render::{Helper, RenderContext, Renderable};
    use crate::support::str::StringWriter;
    use crate::template::Template;
    use crate::testing::TestHandlebars;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[derive(Clone, Copy)]
    struct DummyHelper;

    impl HelperDef for DummyHelper {
        fn call<'reg: 'rc, 'rc>(
            &self,
            h: &Helper<'rc>,
            r: &'reg Registry<'reg>,
            ctx: &'rc Context,
            rc: &mut RenderContext<'reg, 'rc>,
            out: &mut dyn Output,
        ) -> Result<(), RenderError> {
            h.template().unwrap().render(r, ctx, rc, out)
        }
    }

    static DUMMY_HELPER: DummyHelper = DummyHelper;

    #[test]
    fn test_unregister_helper() {
        let mut r = Registry::new();

        assert!(
            r.register_template_string("dumber", "{{#dumb}}\ndummy helper exists\n{{/dumb}}")
                .is_ok()
        );

        r.register_helper("dumb", Box::new(DUMMY_HELPER));

        assert!(r.render("dumber", &()).is_ok());

        r.unregister_helper("dumb");

        assert!(r.render("dumber", &()).is_err());
        assert!(matches!(
            r.render("dumber", &()).unwrap_err().reason(),
            RenderErrorReason::HelperNotFound(..)
        ));
    }

    #[test]
    fn test_registry_operations() {
        let mut r = Registry::new();

        assert!(r.register_template_string("index", "<h1></h1>").is_ok());

        let tpl = Template::compile("<h2></h2>").unwrap();
        r.register_template("index2", tpl);

        assert_eq!(r.templates.len(), 2);

        r.unregister_template("index");
        assert_eq!(r.templates.len(), 1);

        r.clear_templates();
        assert_eq!(r.templates.len(), 0);

        r.register_helper("dummy", Box::new(DUMMY_HELPER));

        // built-in helpers plus 1
        let num_helpers = 7;
        let num_boolean_helpers = 10; // stuff like gt and lte
        let num_custom_helpers = 1; // dummy from above
        #[cfg(feature = "string_helpers")]
        let string_helpers = 8;
        #[cfg(not(feature = "string_helpers"))]
        let string_helpers = 0;
        assert_eq!(
            r.helpers.len(),
            num_helpers + num_boolean_helpers + num_custom_helpers + string_helpers
        );
    }

    #[test]
    #[cfg(feature = "dir_source")]
    fn test_register_templates_directory() {
        use std::fs::DirBuilder;

        use crate::registry::DirectorySourceOptions;

        let mut r = Registry::new();
        {
            let dir = tempdir().unwrap();

            assert_eq!(r.templates.len(), 0);

            let file1_path = dir.path().join("t1.hbs");
            let mut file1: File = File::create(file1_path).unwrap();
            writeln!(file1, "<h1>Hello {{world}}!</h1>").unwrap();

            let file2_path = dir.path().join("t2.hbs");
            let mut file2: File = File::create(file2_path).unwrap();
            writeln!(file2, "<h1>Hola {{world}}!</h1>").unwrap();

            let file3_path = dir.path().join("t3.hbs");
            let mut file3: File = File::create(file3_path).unwrap();
            writeln!(file3, "<h1>Hallo {{world}}!</h1>").unwrap();

            let file4_path = dir.path().join(".t4.hbs");
            let mut file4: File = File::create(file4_path).unwrap();
            writeln!(file4, "<h1>Hallo {{world}}!</h1>").unwrap();

            r.register_templates_directory(dir.path(), DirectorySourceOptions::default())
                .unwrap();

            assert_eq!(r.templates.len(), 3);
            assert!(r.templates.contains_key("t1"));
            assert!(r.templates.contains_key("t2"));
            assert!(r.templates.contains_key("t3"));
            assert!(!r.templates.contains_key("t4"));

            drop(file1);
            drop(file2);
            drop(file3);

            dir.close().unwrap();
        }

        {
            let dir = tempdir().unwrap();

            let file1_path = dir.path().join("t4.hbs");
            let mut file1: File = File::create(file1_path).unwrap();
            writeln!(file1, "<h1>Hello {{world}}!</h1>").unwrap();

            let file2_path = dir.path().join("t5.erb");
            let mut file2: File = File::create(file2_path).unwrap();
            writeln!(file2, "<h1>Hello {{% world %}}!</h1>").unwrap();

            let file3_path = dir.path().join("t6.html");
            let mut file3: File = File::create(file3_path).unwrap();
            writeln!(file3, "<h1>Hello world!</h1>").unwrap();

            r.register_templates_directory(dir.path(), DirectorySourceOptions::default())
                .unwrap();

            assert_eq!(r.templates.len(), 4);
            assert!(r.templates.contains_key("t4"));

            drop(file1);
            drop(file2);
            drop(file3);

            dir.close().unwrap();
        }

        {
            let dir = tempdir().unwrap();

            DirBuilder::new().create(dir.path().join("french")).unwrap();
            DirBuilder::new()
                .create(dir.path().join("portugese"))
                .unwrap();
            DirBuilder::new()
                .create(dir.path().join("italian"))
                .unwrap();

            let file1_path = dir.path().join("french/t7.hbs");
            let mut file1: File = File::create(file1_path).unwrap();
            writeln!(file1, "<h1>Bonjour {{world}}!</h1>").unwrap();

            let file2_path = dir.path().join("portugese/t8.hbs");
            let mut file2: File = File::create(file2_path).unwrap();
            writeln!(file2, "<h1>Ola {{world}}!</h1>").unwrap();

            let file3_path = dir.path().join("italian/t9.hbs");
            let mut file3: File = File::create(file3_path).unwrap();
            writeln!(file3, "<h1>Ciao {{world}}!</h1>").unwrap();

            r.register_templates_directory(dir.path(), DirectorySourceOptions::default())
                .unwrap();

            assert_eq!(r.templates.len(), 7);
            assert!(r.templates.contains_key("french/t7"));
            assert!(r.templates.contains_key("portugese/t8"));
            assert!(r.templates.contains_key("italian/t9"));

            drop(file1);
            drop(file2);
            drop(file3);

            dir.close().unwrap();
        }

        {
            let dir = tempdir().unwrap();

            let file1_path = dir.path().join("t10.hbs");
            let mut file1: File = File::create(file1_path).unwrap();
            writeln!(file1, "<h1>Bonjour {{world}}!</h1>").unwrap();

            let mut dir_path = dir
                .path()
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if !dir_path.ends_with('/') {
                dir_path.push('/');
            }
            r.register_templates_directory(dir_path, DirectorySourceOptions::default())
                .unwrap();

            assert_eq!(r.templates.len(), 8);
            assert!(r.templates.contains_key("t10"));

            drop(file1);
            dir.close().unwrap();
        }

        {
            let dir = tempdir().unwrap();
            let mut r = Registry::new();

            let file1_path = dir.path().join("t11.hbs.html");
            let mut file1: File = File::create(file1_path).unwrap();
            writeln!(file1, "<h1>Bonjour {{world}}!</h1>").unwrap();

            let mut dir_path = dir
                .path()
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if !dir_path.ends_with('/') {
                dir_path.push('/');
            }
            r.register_templates_directory(
                dir_path,
                DirectorySourceOptions {
                    tpl_extension: ".hbs.html".to_owned(),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(r.templates.len(), 1);
            assert!(r.templates.contains_key("t11"));

            drop(file1);
            dir.close().unwrap();
        }

        {
            let dir = tempdir().unwrap();
            let mut r = Registry::new();

            assert_eq!(r.templates.len(), 0);

            let file1_path = dir.path().join(".t12.hbs");
            let mut file1: File = File::create(file1_path).unwrap();
            writeln!(file1, "<h1>Hello {{world}}!</h1>").unwrap();

            r.register_templates_directory(
                dir.path(),
                DirectorySourceOptions {
                    hidden: true,
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(r.templates.len(), 1);
            assert!(r.templates.contains_key(".t12"));

            drop(file1);

            dir.close().unwrap();
        }
    }

    #[test]
    fn test_render_to_write() {
        let mut r = Registry::new();

        assert!(r.register_template_string("index", "<h1></h1>").is_ok());

        let mut sw = StringWriter::new();
        {
            r.render_to_write("index", &(), &mut sw).ok().unwrap();
        }

        assert_eq!("<h1></h1>".to_string(), sw.into_string());
    }

    #[test]
    fn test_escape_fn() {
        let mut r = Registry::new();

        let input = String::from("\"<>&");

        r.register("test", "{{this}}");

        r.assert_render("test", &input, "&quot;&lt;&gt;&amp;");

        r.register_escape_fn(|s| s.into());

        r.assert_render("test", &input, "\"<>&");

        r.unregister_escape_fn();

        r.assert_render("test", &input, "&quot;&lt;&gt;&amp;");
    }

    #[test]
    fn test_escape() {
        let r = Registry::new();
        let data = json!({"hello": "world"});

        r.assert_render_template(r"\{{hello}}", &data, "{{hello}}");
        r.assert_render_template(r" \{{hello}}", &data, " {{hello}}");
        r.assert_render_template(r"\\{{hello}}", &data, r"\world");
    }

    #[test]
    fn test_strict_mode() {
        let mut r = Registry::new();
        assert!(!r.strict_mode());

        r.set_strict_mode(true);
        assert!(r.strict_mode());

        let data = json!({
            "the_only_key": "the_only_value"
        });

        assert!(
            r.render_template("accessing the_only_key {{the_only_key}}", &data)
                .is_ok()
        );
        assert!(
            r.render_template("accessing non-exists key {{the_key_never_exists}}", &data)
                .is_err()
        );

        let render_error = r
            .render_template("accessing non-exists key {{the_key_never_exists}}", &data)
            .unwrap_err();
        assert_eq!(render_error.column_no.unwrap(), 26);
        assert_eq!(
            match render_error.reason() {
                RenderErrorReason::MissingVariable(path) => path.as_ref().unwrap(),
                _ => unreachable!(),
            },
            "the_key_never_exists"
        );

        let data2 = json!([1, 2, 3]);
        assert!(
            r.render_template("accessing valid array index {{this.[2]}}", &data2)
                .is_ok()
        );
        assert!(
            r.render_template("accessing invalid array index {{this.[3]}}", &data2)
                .is_err()
        );
        let render_error2 = r
            .render_template("accessing invalid array index {{this.[3]}}", &data2)
            .unwrap_err();
        assert_eq!(render_error2.column_no.unwrap(), 31);
        assert_eq!(
            match render_error2.reason() {
                RenderErrorReason::MissingVariable(path) => path.as_ref().unwrap(),
                _ => unreachable!(),
            },
            "this.[3]"
        );
    }

    use crate::json::value::ScopedJson;
    struct GenMissingHelper;
    impl HelperDef for GenMissingHelper {
        fn call_inner<'reg: 'rc, 'rc>(
            &self,
            _: &Helper<'rc>,
            _: &'reg Registry<'reg>,
            _: &'rc Context,
            _: &mut RenderContext<'reg, 'rc>,
        ) -> Result<ScopedJson<'rc>, RenderError> {
            Ok(ScopedJson::Missing)
        }
    }

    #[test]
    fn test_strict_mode_in_helper() {
        let mut r = Registry::new();
        r.set_strict_mode(true);

        r.register_helper(
            "check_missing",
            Box::new(
                |h: &Helper<'_>,
                 _: &Registry<'_>,
                 _: &Context,
                 _: &mut RenderContext<'_, '_>,
                 _: &mut dyn Output|
                 -> Result<(), RenderError> {
                    let value = h.param(0).unwrap();
                    assert!(value.is_value_missing());
                    Ok(())
                },
            ),
        );

        r.register_helper("generate_missing_value", Box::new(GenMissingHelper));

        let data = json!({
            "the_key_we_have": "the_value_we_have"
        });
        assert!(
            r.render_template("accessing non-exists key {{the_key_we_dont_have}}", &data)
                .is_err()
        );
        assert!(
            r.render_template(
                "accessing non-exists key from helper {{check_missing the_key_we_dont_have}}",
                &data
            )
            .is_ok()
        );
        assert!(
            r.render_template(
                "accessing helper that generates missing value {{generate_missing_value}}",
                &data
            )
            .is_err()
        );
    }

    #[test]
    fn test_html_expression() {
        let reg = Registry::new();
        assert_eq!(
            reg.render_template("{{{ a }}}", &json!({"a": "<b>bold</b>"}))
                .unwrap(),
            "<b>bold</b>"
        );
        assert_eq!(
            reg.render_template("{{ &a }}", &json!({"a": "<b>bold</b>"}))
                .unwrap(),
            "<b>bold</b>"
        );
    }

    #[test]
    fn test_render_context() {
        let mut reg = Registry::new();

        let data = json!([0, 1, 2, 3]);

        assert_eq!(
            "0123",
            reg.render_template_with_context(
                "{{#each this}}{{this}}{{/each}}",
                &Context::wraps(&data).unwrap()
            )
            .unwrap()
        );

        reg.register_template_string("t0", "{{#each this}}{{this}}{{/each}}")
            .unwrap();
        assert_eq!(
            "0123",
            reg.render_with_context("t0", &Context::from(data)).unwrap()
        );
    }

    #[test]
    fn test_keys_starts_with_null() {
        env_logger::init();
        let reg = Registry::new();
        let data = json!({
            "optional": true,
            "is_null": true,
            "nullable": true,
            "null": true,
            "falsevalue": true,
        });
        assert_eq!(
            "optional: true --> true",
            reg.render_template(
                "optional: {{optional}} --> {{#if optional }}true{{else}}false{{/if}}",
                &data
            )
            .unwrap()
        );
        assert_eq!(
            "is_null: true --> true",
            reg.render_template(
                "is_null: {{is_null}} --> {{#if is_null }}true{{else}}false{{/if}}",
                &data
            )
            .unwrap()
        );
        assert_eq!(
            "nullable: true --> true",
            reg.render_template(
                "nullable: {{nullable}} --> {{#if nullable }}true{{else}}false{{/if}}",
                &data
            )
            .unwrap()
        );
        assert_eq!(
            "falsevalue: true --> true",
            reg.render_template(
                "falsevalue: {{falsevalue}} --> {{#if falsevalue }}true{{else}}false{{/if}}",
                &data
            )
            .unwrap()
        );
        assert_eq!(
            "null: true --> false",
            reg.render_template(
                "null: {{null}} --> {{#if null }}true{{else}}false{{/if}}",
                &data
            )
            .unwrap()
        );
        assert_eq!(
            "null: true --> true",
            reg.render_template(
                "null: {{null}} --> {{#if this.[null]}}true{{else}}false{{/if}}",
                &data
            )
            .unwrap()
        );
    }

    #[test]
    fn test_dev_mode_template_reload() {
        let mut reg = Registry::new();
        reg.set_dev_mode(true);
        assert!(reg.dev_mode());

        let dir = tempdir().unwrap();
        let file1_path = dir.path().join("t1.hbs");
        {
            let mut file1: File = File::create(&file1_path).unwrap();
            write!(file1, "<h1>Hello {{{{name}}}}!</h1>").unwrap();
        }

        reg.register_template_file("t1", &file1_path).unwrap();

        assert_eq!(
            reg.render("t1", &json!({"name": "Alex"})).unwrap(),
            "<h1>Hello Alex!</h1>"
        );

        {
            let mut file1: File = File::create(&file1_path).unwrap();
            write!(file1, "<h1>Privet {{{{name}}}}!</h1>").unwrap();
        }

        assert_eq!(
            reg.render("t1", &json!({"name": "Alex"})).unwrap(),
            "<h1>Privet Alex!</h1>"
        );

        dir.close().unwrap();
    }

    #[test]
    fn test_template_package_mutual_reference() {
        let mut r = Registry::new();

        // a template outside the package can be referenced by package members
        r.register_template_string("shared/footer", "(footer)")
            .unwrap();

        r.register_template_package([
            (
                "page",
                "<html>{{> header}}|{{> content}}|{{> shared/footer}}</html>",
            ),
            ("header", "HEADER"),
            // package members may reference each other, in any order
            ("content", "CONTENT[{{> aside}}]"),
            ("aside", "ASIDE"),
        ])
        .unwrap();

        assert_eq!(
            r.render("page", &()).unwrap(),
            "<html>HEADER|CONTENT[ASIDE]|(footer)</html>"
        );
        assert!(r.has_template("page"));
        assert!(r.has_template("aside"));
    }

    #[test]
    fn test_template_package_compile_failure_rolls_back() {
        let mut r = Registry::new();

        r.register_template_package([("index", "v1 {{who}}"), ("panel", "PANEL")])
            .unwrap();
        assert_eq!(r.render("index", &json!({"who": "one"})).unwrap(), "v1 one");

        // two broken members and one good one; the update must fail and
        // report *both* broken members in stable (sorted) order
        let err = r
            .register_template_package([
                ("z_broken", "{{#if}}{{/each}}"),
                ("a_broken", "{{#each}}{{/if}}"),
                ("index", "v2 {{who}}"),
                ("brand_new", "NEW"),
            ])
            .unwrap_err();
        assert_eq!(err.template_names(), vec!["a_broken", "z_broken"]);

        // the pre-call state is fully preserved: old members still render
        // the old content, and no new member leaked in
        assert_eq!(r.render("index", &json!({"who": "one"})).unwrap(), "v1 one");
        assert_eq!(r.render("panel", &()).unwrap(), "PANEL");
        assert!(!r.has_template("brand_new"));
        assert!(!r.has_template("a_broken"));
        assert!(!r.has_template("z_broken"));
    }

    #[test]
    fn test_template_package_unresolved_partial_rolls_back() {
        let mut r = Registry::new();

        r.register_template_package([("page", "v1 {{> sidebar}}"), ("sidebar", "SIDE")])
            .unwrap();

        // v2 drops "sidebar" but "page" still references it: the package is
        // no longer dependency-closed and must be rejected as a whole
        let err = r
            .register_template_package([("page", "v2 {{> sidebar}}")])
            .unwrap_err();
        assert_eq!(err.template_names(), vec!["page"]);
        assert!(matches!(
            err,
            crate::TemplatePackageError::UnresolvedPartials(_)
        ));

        assert_eq!(r.render("page", &()).unwrap(), "v1 SIDE");
        assert!(r.has_template("sidebar"));
    }

    #[test]
    fn test_template_package_member_removal() {
        let mut r = Registry::new();

        // templates outside the package are not touched by package updates
        r.register_template_string("standalone", "STANDALONE")
            .unwrap();

        r.register_template_package([("keep", "KEEP"), ("drop", "DROP")])
            .unwrap();
        assert!(r.has_template("drop"));

        r.register_template_package([("keep", "KEEP2")]).unwrap();

        assert_eq!(r.render("keep", &()).unwrap(), "KEEP2");
        assert_eq!(r.render("standalone", &()).unwrap(), "STANDALONE");
        assert!(!r.has_template("drop"));
        assert!(matches!(
            r.render("drop", &()).unwrap_err().reason(),
            RenderErrorReason::TemplateNotFound(_)
        ));
    }

    #[test]
    fn test_template_package_idempotent_replay() {
        let mut r = Registry::new();

        let package = [("a", "A {{> b}}"), ("b", "B")];
        r.register_template_package(package).unwrap();
        let first = r.render("a", &()).unwrap();
        let count = r.get_templates().len();

        // replaying the exact same content succeeds and changes nothing
        r.register_template_package(package).unwrap();
        assert_eq!(r.render("a", &()).unwrap(), first);
        assert_eq!(r.get_templates().len(), count);

        // replaying after an unrelated render still works
        assert_eq!(r.render("a", &()).unwrap(), "A B");
        r.register_template_package(package).unwrap();
        assert_eq!(r.render("a", &()).unwrap(), "A B");
    }

    #[test]
    fn test_template_package_dev_mode_file_source() {
        let mut r = Registry::new();
        r.set_dev_mode(true);

        let dir = tempdir().unwrap();
        let page_path = dir.path().join("page.hbs");
        let part_path = dir.path().join("part.hbs");
        {
            let mut f: File = File::create(&page_path).unwrap();
            write!(f, "PAGE[{{{{> part}}}}]").unwrap();
        }
        {
            let mut f: File = File::create(&part_path).unwrap();
            write!(f, "PART-V1").unwrap();
        }

        r.register_template_package_files([
            ("page", page_path.clone()),
            ("part", part_path.clone()),
        ])
        .unwrap();
        assert_eq!(r.render("page", &()).unwrap(), "PAGE[PART-V1]");

        // dev mode reloads package members from their files
        {
            let mut f: File = File::create(&part_path).unwrap();
            write!(f, "PART-V2").unwrap();
        }
        assert_eq!(r.render("page", &()).unwrap(), "PAGE[PART-V2]");

        // a package update that drops "part" must also drop its dev-mode
        // source: the old file must not resurrect the removed member
        r.register_template_package([("page", "PAGE-ONLY")])
            .unwrap();
        assert_eq!(r.render("page", &()).unwrap(), "PAGE-ONLY");
        assert!(!r.has_template("part"));
        assert!(matches!(
            r.render("part", &()).unwrap_err().reason(),
            RenderErrorReason::TemplateNotFound(_)
        ));

        // and a failed update leaves the dev-mode sources untouched
        {
            let mut f: File = File::create(&page_path).unwrap();
            write!(f, "PAGE[{{{{> part}}}}]").unwrap();
        }
        assert!(
            r.register_template_package([("page", "{{#if}}{{/each}}")])
                .is_err()
        );
        assert_eq!(r.render("page", &()).unwrap(), "PAGE-ONLY");

        dir.close().unwrap();
    }

    #[test]
    #[cfg(feature = "script_helper")]
    fn test_script_helper() {
        let mut reg = Registry::new();

        reg.register_script_helper("acc", "params.reduce(|sum, x| x + sum, 0)")
            .unwrap();

        assert_eq!(
            reg.render_template("{{acc 1 2 3 4}}", &json!({})).unwrap(),
            "10"
        );
    }

    #[test]
    #[cfg(feature = "script_helper")]
    fn test_script_helper_dev_mode() {
        let mut reg = Registry::new();
        reg.set_dev_mode(true);

        let dir = tempdir().unwrap();
        let file1_path = dir.path().join("acc.rhai");
        {
            let mut file1: File = File::create(&file1_path).unwrap();
            write!(file1, "params.reduce(|sum, x| x + sum, 0)").unwrap();
        }

        reg.register_script_helper_file("acc", &file1_path).unwrap();

        assert_eq!(
            reg.render_template("{{acc 1 2 3 4}}", &json!({})).unwrap(),
            "10"
        );

        {
            let mut file1: File = File::create(&file1_path).unwrap();
            write!(file1, "params.reduce(|sum, x| x * sum, 1)").unwrap();
        }

        assert_eq!(
            reg.render_template("{{acc 1 2 3 4}}", &json!({})).unwrap(),
            "24"
        );

        dir.close().unwrap();
    }

    #[test]
    #[cfg(feature = "script_helper")]
    fn test_engine_access() {
        use rhai::Engine;

        let mut registry = Registry::new();
        let mut eng = Engine::new();
        eng.set_max_string_size(1000);
        registry.set_engine(eng);

        assert_eq!(1000, registry.engine().max_string_size());
    }
}

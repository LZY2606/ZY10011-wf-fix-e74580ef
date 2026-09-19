extern crate handlebars;
#[macro_use]
extern crate serde_json;

use std::fs;
use std::path::PathBuf;

use handlebars::{Handlebars, RenderErrorReason, TemplateSource, TemplateUpdateMemberError};
use tempfile::tempdir;

fn member_names(err: &handlebars::TemplateUpdateError) -> Vec<&str> {
    err.members.keys().map(String::as_str).collect()
}

#[test]
fn test_update_templates_cross_reference_package() {
    let mut hbs = Handlebars::new();
    hbs.update_templates([
        ("page", "<html>{{> greeting}}</html>"),
        ("greeting", "Hello {{name}} from {{> footer}}"),
        ("footer", "the team"),
    ])
    .unwrap();

    assert_eq!(
        hbs.render("page", &json!({"name": "world"})).unwrap(),
        "<html>Hello world from the team</html>"
    );
    assert_eq!(
        hbs.render("greeting", &json!({"name": "rust"})).unwrap(),
        "Hello rust from the team"
    );
}

#[test]
fn test_update_templates_rollback_on_compile_failure() {
    let mut hbs = Handlebars::new();
    hbs.update_templates([
        ("page", "<h1>{{> greeting}}</h1>"),
        ("greeting", "Hello {{name}}"),
    ])
    .unwrap();

    let err = hbs
        .update_templates([
            ("page", "{{#if oops}}"),
            ("greeting", "{{#each oops}}"),
            ("extra", "fine"),
        ])
        .unwrap_err();

    assert_eq!(member_names(&err), vec!["greeting", "page"]);
    assert!(matches!(
        err.members.get("page").unwrap(),
        TemplateUpdateMemberError::Template(_)
    ));

    // old package is still fully visible
    assert_eq!(
        hbs.render("page", &json!({"name": "world"})).unwrap(),
        "<h1>Hello world</h1>"
    );
    assert_eq!(
        hbs.render("greeting", &json!({"name": "world"})).unwrap(),
        "Hello world"
    );
    assert!(matches!(
        hbs.render("extra", &json!({}))
            .unwrap_err()
            .reason(),
        RenderErrorReason::TemplateNotFound(name) if name == "extra"
    ));
}

#[test]
fn test_update_templates_reports_missing_dependency() {
    let mut hbs = Handlebars::new();
    let err = hbs
        .update_templates([
            ("page", "{{> header}} and {{> footer}}"),
            ("header", "head"),
        ])
        .unwrap_err();

    assert_eq!(member_names(&err), vec!["page"]);
    match err.members.get("page").unwrap() {
        TemplateUpdateMemberError::MissingDependency {
            name,
            referenced_by,
        } => {
            assert_eq!(name, "footer");
            assert_eq!(referenced_by, &["page".to_owned()]);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // nothing was committed
    assert!(matches!(
        hbs.render("page", &json!({}))
            .unwrap_err()
            .reason(),
        RenderErrorReason::TemplateNotFound(name) if name == "page"
    ));
}

#[test]
fn test_update_templates_dependency_missing_from_both_reported_stably() {
    let mut hbs = Handlebars::new();
    // both members miss the same dependency; each must be listed
    let err = hbs
        .update_templates([("a", "{{> missing}}"), ("b", "{{> missing}}")])
        .unwrap_err();

    assert_eq!(member_names(&err), vec!["a", "b"]);
    for name in ["a", "b"] {
        match err.members.get(name).unwrap() {
            TemplateUpdateMemberError::MissingDependency {
                name: dependency,
                referenced_by,
            } => {
                assert_eq!(dependency, "missing");
                assert_eq!(referenced_by, &["a".to_owned(), "b".to_owned()]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}

#[test]
fn test_update_templates_removes_deleted_members() {
    let mut hbs = Handlebars::new();
    hbs.update_templates([
        ("page", "{{> old_partial}}"),
        ("old_partial", "old"),
        ("other", "other"),
    ])
    .unwrap();

    // new package drops page and old_partial
    hbs.update_templates([("other", "other"), ("new_partial", "new")])
        .unwrap();

    assert_eq!(hbs.render("other", &()).unwrap(), "other");
    assert_eq!(hbs.render("new_partial", &()).unwrap(), "new");
    for removed in ["page", "old_partial"] {
        assert!(matches!(
            hbs.render(removed, &()).unwrap_err().reason(),
            RenderErrorReason::TemplateNotFound(name) if name == removed
        ));
        assert!(!hbs.has_template(removed));
    }
}

#[test]
fn test_update_templates_is_idempotent_for_same_content() {
    let mut hbs = Handlebars::new();
    let package = [
        ("page", "<h1>{{> greeting}}</h1>"),
        ("greeting", "Hello {{name}}"),
    ];

    hbs.update_templates(package).unwrap();
    let first = hbs.render("page", &json!({"name": "world"})).unwrap();

    // replay the exact same content: succeeds and renders identically
    hbs.update_templates(package).unwrap();
    let second = hbs.render("page", &json!({"name": "world"})).unwrap();
    assert_eq!(first, second);
    assert_eq!(hbs.get_templates().len(), 2);
}

#[test]
fn test_update_templates_dev_mode_deleted_source_not_reloaded() {
    let mut hbs = Handlebars::new();
    hbs.set_dev_mode(true);

    let dir = tempdir().unwrap();
    let keep_path = dir.path().join("keep.hbs");
    let drop_path = dir.path().join("drop.hbs");
    fs::write(&keep_path, "keep {{name}}").unwrap();
    fs::write(&drop_path, "drop {{name}}").unwrap();

    hbs.update_templates([
        ("keep", TemplateSource::File(keep_path.clone())),
        ("drop", TemplateSource::File(drop_path.clone())),
    ])
    .unwrap();
    assert_eq!(hbs.render("drop", &json!({"name": "x"})).unwrap(), "drop x");

    // mutate the old file on disk, then submit a package without "drop"
    fs::write(&drop_path, "STALE {{name}}").unwrap();
    hbs.update_templates([("keep", TemplateSource::File(keep_path.clone()))])
        .unwrap();

    // deleted member must not be secretly served from its old source
    assert!(!hbs.has_template("drop"));
    assert!(matches!(
        hbs.render("drop", &json!({"name": "x"})).unwrap_err().reason(),
        RenderErrorReason::TemplateNotFound(name) if name == "drop"
    ));

    // remaining member still tracks its file
    fs::write(&keep_path, "kept {{name}}").unwrap();
    assert_eq!(hbs.render("keep", &json!({"name": "y"})).unwrap(), "kept y");

    dir.close().unwrap();
}

#[test]
fn test_update_templates_dev_mode_broken_package_does_not_touch_sources() {
    let mut hbs = Handlebars::new();
    hbs.set_dev_mode(true);

    let dir = tempdir().unwrap();
    let page_path = dir.path().join("page.hbs");
    fs::write(&page_path, "v1 {{name}}").unwrap();

    hbs.update_templates([("page", TemplateSource::File(page_path.clone()))])
        .unwrap();

    let missing_path = dir.path().join("missing.hbs");
    // file does not exist: loading fails and the old package stays live
    let err = hbs
        .update_templates([
            ("page", TemplateSource::File(page_path.clone())),
            ("ghost", TemplateSource::File(missing_path)),
        ])
        .unwrap_err();
    assert_eq!(member_names(&err), vec!["ghost"]);
    assert!(matches!(
        err.members.get("ghost").unwrap(),
        TemplateUpdateMemberError::Template(_)
    ));

    assert_eq!(hbs.render("page", &json!({"name": "x"})).unwrap(), "v1 x");
    assert!(!hbs.has_template("ghost"));

    dir.close().unwrap();
}

#[test]
fn test_update_templates_rejects_duplicate_names() {
    let mut hbs = Handlebars::new();
    let err = hbs
        .update_templates([("a", "one"), ("b", "two"), ("a", "three")])
        .unwrap_err();
    assert_eq!(member_names(&err), vec!["a"]);
    assert!(matches!(
        err.members.get("a").unwrap(),
        TemplateUpdateMemberError::DuplicateName
    ));
    assert!(!hbs.has_template("a"));
    assert!(!hbs.has_template("b"));
}

#[test]
fn test_update_templates_accepts_path_buf_pairs() {
    let mut hbs = Handlebars::new();
    let dir = tempdir().unwrap();
    let path: PathBuf = dir.path().join("t.hbs");
    fs::write(&path, "file {{name}}").unwrap();

    // plain PathBuf pairs convert without mentioning TemplateSource
    hbs.update_templates(vec![("t", path)]).unwrap();
    assert_eq!(hbs.render("t", &json!({"name": "z"})).unwrap(), "file z");

    dir.close().unwrap();
}

#[test]
fn test_update_templates_keeps_strict_mode_and_escape() {
    let mut hbs = Handlebars::new();
    hbs.set_strict_mode(true);
    hbs.update_templates([("t", "{{missing}}")]).unwrap();

    assert!(hbs.strict_mode());
    assert!(matches!(
        hbs.render("t", &json!({})).unwrap_err().reason(),
        RenderErrorReason::MissingVariable(Some(name)) if name == "missing"
    ));

    // default html escaping still applies
    hbs.update_templates([("t", "{{this}}")]).unwrap();
    assert_eq!(hbs.render("t", &"<b>").unwrap(), "&lt;b&gt;");
}

#[test]
fn test_update_templates_preserves_helpers() {
    use handlebars::{Context, Helper, HelperDef, HelperResult, Output, RenderContext, Renderable};

    #[derive(Clone, Copy)]
    struct Shout;
    impl HelperDef for Shout {
        fn call<'reg: 'rc, 'rc>(
            &self,
            h: &Helper<'rc>,
            r: &'reg Handlebars<'reg>,
            ctx: &'rc Context,
            rc: &mut RenderContext<'reg, 'rc>,
            out: &mut dyn Output,
        ) -> HelperResult {
            h.template().unwrap().render(r, ctx, rc, out)?;
            out.write("!")?;
            Ok(())
        }
    }

    let mut hbs = Handlebars::new();
    hbs.register_helper("shout", Box::new(Shout));
    hbs.update_templates([("t", "{{#shout}}hi{{/shout}}")])
        .unwrap();
    assert_eq!(hbs.render("t", &()).unwrap(), "hi!");

    // helper survives another package swap
    hbs.update_templates([("t", "x")]).unwrap();
    hbs.update_templates([("t", "{{#shout}}yo{{/shout}}")])
        .unwrap();
    assert_eq!(hbs.render("t", &()).unwrap(), "yo!");
}

use errors::AppError;
use serde_json;
use slog::Logger;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::File;
use std::io::BufReader;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Settings {
  pub workspace: String,
  pub shell: Option<Vec<String>>,
  pub default_after_workon: Option<String>,
  pub default_after_clone: Option<String>,
  pub default_tags: Option<BTreeSet<String>>,
  pub tags: Option<BTreeMap<String, Tag>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Tag {
  pub after_clone: Option<String>,
  pub after_workon: Option<String>,
  pub priority: Option<u8>,
  pub workspace: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Project {
  pub name: String,
  pub git: String,
  pub after_clone: Option<String>,
  pub after_workon: Option<String>,
  pub override_path: Option<String>,
  pub tags: Option<BTreeSet<String>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
  pub projects: BTreeMap<String, Project>,
  pub settings: Settings,
}

impl Project {
  fn check_sanity(&self, config: &Config, logger: &Logger) -> Result<(), AppError> {
    let sanity_logger = logger.new(o!("task" => "check_sanity"));
    let path = config.actual_path_to_project(self, &sanity_logger);
    if path.is_absolute() {
      Ok(())
    } else {
      Err(AppError::UserError(format!(
        "Misconfigured project {}: resolved path {:?} is relative which is not allowed",
        &self.name,
        &path
      )))
    }
  }
}

impl Config {
  pub fn actual_path_to_project(&self, project: &Project, logger: &Logger) -> PathBuf {
    let path = project.override_path
                      .clone()
                      .map(PathBuf::from)
                      .unwrap_or_else(|| {
      Path::new(self.resolve_workspace(logger, project).as_str()).join(project.name.as_str())
    });
    expand_path(path)
  }

  fn resolve_workspace(&self, logger: &Logger, project: &Project) -> String {
    let x = self.resolve_from_tags(
      |tag| tag.workspace.clone(),
      // TODO @mriehl last without mutation?
      |mut workspaces_from_tags| {
        workspaces_from_tags.pop().expect(
          "joiner only used if resolved vec is not empty",
        )
      },
      project.tags.clone(),
      logger,
    );
    let workspace = x.unwrap_or_else(|| self.settings.workspace.clone());
    trace!(logger, "resolved"; "workspace" => workspace);
    workspace
  }
  pub fn resolve_after_clone(&self, logger: &Logger, project: &Project) -> Option<String> {
    project.after_clone.clone().or_else(|| {
      self.resolve_after_clone_from_tags(project.tags.clone(), logger)
    })
  }
  pub fn resolve_after_workon(&self, logger: &Logger, project: &Project) -> String {
    project.after_workon
           .clone()
           .or_else(|| {
      self.resolve_workon_from_tags(project.tags.clone(), logger)
    })
           .map(|c| prepare_workon(&c))
           .unwrap_or_else(|| "".to_owned())
  }

  fn check_sanity(self, logger: &Logger) -> Result<Config, AppError> {
    for project in self.projects.values() {
      project.check_sanity(&self, logger)?
    }
    Ok(self)
  }

  fn resolve_workon_from_tags(&self, maybe_tags: Option<BTreeSet<String>>, logger: &Logger) -> Option<String> {
    self.resolve_from_tags(
      |t| t.clone().after_workon,
      |v| v.join(" && "),
      maybe_tags,
      logger,
    )
  }
  fn resolve_after_clone_from_tags(&self, maybe_tags: Option<BTreeSet<String>>, logger: &Logger) -> Option<String> {
    self.resolve_from_tags(
      |t| t.clone().after_clone,
      |v| v.join(" && "),
      maybe_tags,
      logger,
    )
  }

  fn tag_priority_or_fallback(&self, name: &str, tag: &Tag, logger: &Logger) -> u8 {
    match tag.priority {
    None => {
      debug!(logger, r#"No tag priority set, will use default (50).
Tags with low priority are applied first and if they all have the same priority
they will be applied in alphabetical name order so it is recommended you make a
conscious choice and set the value."#;
            "tag_name" => name, "tag_def" => format!("{:?}", tag));
      50
    }
    Some(p) => p,
    }
  }

  fn resolve_from_tags<F, J>(&self, resolver: F, joiner: J, maybe_tags: Option<BTreeSet<String>>, logger: &Logger) -> Option<String>
  where
    F: Fn(&Tag) -> Option<String>,
    J: Fn(Vec<String>) -> String,
  {
    let tag_logger = logger.new(o!("tags" => format!("{:?}", maybe_tags)));
    trace!(tag_logger, "Resolving");
    if maybe_tags.is_none() || self.settings.tags.is_none() {
      None
    } else {
      let tags: BTreeSet<String> = maybe_tags.unwrap();
      let settings_tags = self.clone().settings.tags.unwrap();
      let mut resolved_with_priority: Vec<(String, u8)> = tags.iter()
                                                              .flat_map(|t| match settings_tags.get(t) {
      None => {
        warn!(tag_logger, "Ignoring tag since it was not found in the config"; "missing_tag" => t.clone());
        None
      }
      Some(actual_tag) => {
        resolver(actual_tag).clone().map(|val| {
          (val, self.tag_priority_or_fallback(t, actual_tag, logger))
        })
      }
      })
                                                              .collect();
      trace!(logger, "before sort"; "tags" => format!("{:?}", resolved_with_priority));
      resolved_with_priority.sort_by_key(|resolved_and_priority| resolved_and_priority.1);
      trace!(logger, "after sort"; "tags" => format!("{:?}", resolved_with_priority));
      let resolved: Vec<String> = resolved_with_priority.into_iter().map(|r| r.0).collect();
      if resolved.is_empty() {
        None
      } else {
        let resolved_cmd = joiner(resolved);
        debug!(tag_logger, format!("resolved {:?}", resolved_cmd));
        Some(resolved_cmd)
      }
    }
  }
}

fn prepare_workon(workon: &str) -> String {
  format!(" && {}", workon)
}

fn read_config<R>(reader: Result<R, AppError>, logger: &Logger) -> Result<Config, AppError>
where
  R: Read,
{
  reader.and_then(|r| {
    serde_json::de::from_reader(r).map_err(AppError::BadJson)
  })
        .and_then(|c: Config| c.check_sanity(logger))
}

fn default_config_path() -> Result<PathBuf, AppError> {
  let mut home: PathBuf = env::home_dir().ok_or_else(|| {
    AppError::UserError("$HOME not set".to_owned())
  })?;
  home.push(".fw.json");
  Ok(home)
}

pub fn actual_config_path(maybe_config_override: Option<&str>) -> Result<PathBuf, AppError> {
  let maybe_config: Option<Result<PathBuf, AppError>> = maybe_config_override.map(|path| Ok(PathBuf::from(path)));
  maybe_config.unwrap_or_else(default_config_path)
}

fn determine_config(maybe_config_override: Option<&str>) -> Result<File, AppError> {
  let config_file_path = actual_config_path(maybe_config_override)?;
  let path = config_file_path.to_str().ok_or_else(|| {
    AppError::UserError("$HOME is not valid utf8".to_owned())
  });
  path.and_then(|path| File::open(path).map_err(AppError::IO))
}

pub fn get_config(logger: &Logger, maybe_config_override: Option<&str>) -> Result<Config, AppError> {
  let config_file = determine_config(maybe_config_override);
  let reader = config_file.map(BufReader::new);
  read_config(reader, logger)
}

fn repo_name_from_url(url: &str) -> Result<&str, AppError> {
  let last_fragment = url.rsplit('/').next().ok_or_else(|| {
    AppError::UserError(format!(
      "Given URL {} does not have path fragments so cannot determine project name. Please give \
                                                                    one.",
      url
    ))
  })?;

  // trim_right_matches is more efficient but would fuck us up with repos like git@github.com:bauer/test.git.git (which is legal)
  Ok(if last_fragment.ends_with(".git") {
    last_fragment.split_at(last_fragment.len() - 4).0
  } else {
    last_fragment
  })
}

pub fn add_entry(
  maybe_config: Result<Config, AppError>,
  maybe_name: Option<&str>,
  url: &str,
  logger: &Logger,
  maybe_config_override: Option<&str>,
) -> Result<(), AppError> {
  let name = maybe_name.ok_or_else(|| {
    AppError::UserError(format!("No project name specified for {}", url))
  })
                       .or_else(|_| repo_name_from_url(url))?;
  let mut config: Config = maybe_config?;
  info!(logger, "Prepare new project entry"; "name" => name, "url" => url);
  if config.projects.contains_key(name) {
    Err(AppError::UserError(format!(
      "Project key {} already exists, not gonna overwrite it for you",
      name
    )))
  } else {
    config.projects.insert(
      name.to_owned(),
      Project {
        git: url.to_owned(),
        name: name.to_owned(),
        after_clone: config.settings.default_after_clone.clone(),
        after_workon: config.settings.default_after_workon.clone(),
        override_path: None,
        tags: config.settings.default_tags.clone(),
      },
    );
    info!(logger, "Updated config"; "config" => format!("{:?}", config));
    write_config(config, logger, maybe_config_override)
  }
}

pub fn update_entry(
  maybe_config: Result<Config, AppError>,
  name: &str,
  git: Option<String>,
  after_workon: Option<String>,
  after_clone: Option<String>,
  override_path: Option<String>,
  logger: &Logger,
  maybe_config_override: Option<&str>,
) -> Result<(), AppError> {
  let mut config: Config = maybe_config?;
  info!(logger, "Update project entry"; "name" => name);
  if name.starts_with("http") || name.starts_with("git@") {
    Err(AppError::UserError(format!(
      "{} looks like a repo URL and not like a project name, please fix",
      name
    )))
  } else if !config.projects.contains_key(name) {
    Err(AppError::UserError(format!(
      "Project key {} does not exists. Can not update.",
      name
    )))
  } else {
    let old_project_config: Project = config.projects
                                            .get(name)
                                            .expect("Already checked in the if above")
                                            .clone();
    config.projects.insert(
      name.to_owned(),
      Project {
        git: git.unwrap_or(old_project_config.git),
        name: old_project_config.name,
        after_clone: after_clone.or(old_project_config.after_clone),
        after_workon: after_workon.or(old_project_config.after_workon),
        override_path: override_path.or(old_project_config.override_path),
        tags: None,
      },
    );
    debug!(logger, "Updated config"; "config" => format!("{:?}", config));
    write_config(config, logger, maybe_config_override)
  }
}

pub fn write_config(config: Config, logger: &Logger, maybe_config_override: Option<&str>) -> Result<(), AppError> {
  let config_path = actual_config_path(maybe_config_override)?;
  info!(logger, "Writing config"; "path" => format!("{:?}", config_path));
  config.check_sanity(logger).and_then(|c| {
    let mut buffer = File::create(config_path)?;
    serde_json::ser::to_writer_pretty(&mut buffer, &c).map_err(AppError::BadJson)
  })
}

fn do_expand(path: PathBuf, home_dir: Option<PathBuf>) -> PathBuf {
  if let Some(home) = home_dir {
    home.join(path.strip_prefix("~").expect(
      "only doing this if path starts with ~",
    ))
  } else {
    path
  }
}

pub fn expand_path(path: PathBuf) -> PathBuf {
  if path.starts_with("~") {
    do_expand(path, env::home_dir())
  } else {
    path
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use spectral::prelude::*;

  #[test]
  fn test_repo_name_from_url() {
    let https_url = "https://github.com/mriehl/fw";
    let name = repo_name_from_url(&https_url).unwrap().to_owned();
    assert_that(&name).is_equal_to("fw".to_owned());
  }
  #[test]
  fn test_repo_name_from_ssh_pragma() {
    let ssh_pragma = "git@github.com:mriehl/fw.git";
    let name = repo_name_from_url(&ssh_pragma).unwrap().to_owned();
    assert_that(&name).is_equal_to("fw".to_owned());
  }
  #[test]
  fn test_repo_name_from_ssh_pragma_with_multiple_git_endings() {
    let ssh_pragma = "git@github.com:mriehl/fw.git.git";
    let name = repo_name_from_url(&ssh_pragma).unwrap().to_owned();
    assert_that(&name).is_equal_to("fw.git".to_owned());
  }
  #[test]
  fn test_do_not_expand_path_without_tilde() {
    let path = PathBuf::from("/foo/bar");
    assert_that(&expand_path(path.clone())).is_equal_to(&path);
  }
  #[test]
  fn test_do_expand_path() {
    let path = PathBuf::from("~/foo/bar");
    let home = PathBuf::from("/my/home");
    assert_that(&do_expand(path, Some(home))).is_equal_to(PathBuf::from("/my/home/foo/bar"));
  }
  #[test]
  fn test_workon_from_tags() {
    let config = a_config();
    let logger = a_logger();
    let resolved = config.resolve_after_workon(&logger, config.projects.get("test1").unwrap());
    assert_that(&resolved).is_equal_to(" && workon1 && workon2".to_owned());
  }
  #[test]
  fn test_workon_from_tags_prioritized() {
    let config = a_config();
    let logger = a_logger();
    let resolved = config.resolve_after_workon(&logger, config.projects.get("test5").unwrap());
    assert_that(&resolved).is_equal_to(" && workon4 && workon3".to_owned());
  }
  #[test]
  fn test_after_clone_from_tags() {
    let config = a_config();
    let logger = a_logger();
    let resolved = config.resolve_after_clone(&logger, config.projects.get("test1").unwrap());
    assert_that(&resolved).is_equal_to(Some("clone1 && clone2".to_owned()));
  }
  #[test]
  fn test_after_clone_from_tags_prioritized() {
    let config = a_config();
    let logger = a_logger();
    let resolved = config.resolve_after_clone(&logger, config.projects.get("test5").unwrap());
    assert_that(&resolved).is_equal_to(Some("clone4 && clone3".to_owned()));
  }
  #[test]
  fn test_workon_from_tags_missing_one_tag_graceful() {
    let config = a_config();
    let logger = a_logger();
    let resolved = config.resolve_after_workon(&logger, config.projects.get("test2").unwrap());
    assert_that(&resolved).is_equal_to(" && workon1".to_owned());
  }
  #[test]
  fn test_workon_from_tags_missing_all_tags_graceful() {
    let config = a_config();
    let logger = a_logger();
    let resolved = config.resolve_after_workon(&logger, config.projects.get("test4").unwrap());
    assert_that(&resolved).is_equal_to("".to_owned());
  }
  #[test]
  fn test_after_clone_from_tags_missing_all_tags_graceful() {
    let config = a_config();
    let logger = a_logger();
    let resolved = config.resolve_after_clone(&logger, config.projects.get("test4").unwrap());
    assert_that(&resolved).is_equal_to(None);
  }
  #[test]
  fn test_after_clone_from_tags_missing_one_tag_graceful() {
    let config = a_config();
    let logger = a_logger();
    let resolved = config.resolve_after_clone(&logger, config.projects.get("test2").unwrap());
    assert_that(&resolved).is_equal_to(Some("clone1".to_owned()));
  }
  #[test]
  fn test_workon_override_from_project() {
    let config = a_config();
    let logger = a_logger();
    let resolved = config.resolve_after_workon(&logger, config.projects.get("test3").unwrap());
    assert_that(&resolved).is_equal_to(" && workon override in project".to_owned());
  }
  #[test]
  fn test_after_clone_override_from_project() {
    let config = a_config();
    let logger = a_logger();
    let resolved = config.resolve_after_clone(&logger, config.projects.get("test3").unwrap());
    assert_that(&resolved).is_equal_to(Some("clone override in project".to_owned()));
  }

  fn a_config() -> Config {
    let project = Project {
      name: "test1".to_owned(),
      git: "irrelevant".to_owned(),
      tags: Some(btreeset!["tag1".to_owned(), "tag2".to_owned()]),
      after_clone: None,
      after_workon: None,
      override_path: None,
    };
    let project2 = Project {
      name: "test2".to_owned(),
      git: "irrelevant".to_owned(),
      tags: Some(btreeset![
        "tag1".to_owned(),
        "tag-does-not-exist".to_owned(),
      ]),
      after_clone: None,
      after_workon: None,
      override_path: None,
    };
    let project3 = Project {
      name: "test3".to_owned(),
      git: "irrelevant".to_owned(),
      tags: Some(btreeset!["tag1".to_owned()]),
      after_clone: Some("clone override in project".to_owned()),
      after_workon: Some("workon override in project".to_owned()),
      override_path: None,
    };
    let project4 = Project {
      name: "test4".to_owned(),
      git: "irrelevant".to_owned(),
      tags: Some(btreeset!["tag-does-not-exist".to_owned()]),
      after_clone: None,
      after_workon: None,
      override_path: None,
    };
    let project5 = Project {
      name: "test5".to_owned(),
      git: "irrelevant".to_owned(),
      tags: Some(btreeset!["tag3".to_owned(), "tag4".to_owned()]),
      after_clone: None,
      after_workon: None,
      override_path: None,
    };
    let tag1 = Tag {
      after_clone: Some("clone1".to_owned()),
      after_workon: Some("workon1".to_owned()),
      priority: None,
      workspace: None,
    };
    let tag2 = Tag {
      after_clone: Some("clone2".to_owned()),
      after_workon: Some("workon2".to_owned()),
      priority: None,
      workspace: None,
    };
    let tag3 = Tag {
      after_clone: Some("clone3".to_owned()),
      after_workon: Some("workon3".to_owned()),
      priority: Some(100),
      workspace: None,
    };
    let tag4 = Tag {
      after_clone: Some("clone4".to_owned()),
      after_workon: Some("workon4".to_owned()),
      priority: Some(0),
      workspace: None,
    };
    let mut projects: BTreeMap<String, Project> = BTreeMap::new();
    projects.insert("test1".to_owned(), project);
    projects.insert("test2".to_owned(), project2);
    projects.insert("test3".to_owned(), project3);
    projects.insert("test4".to_owned(), project4);
    projects.insert("test5".to_owned(), project5);
    let mut tags: BTreeMap<String, Tag> = BTreeMap::new();
    tags.insert("tag1".to_owned(), tag1);
    tags.insert("tag2".to_owned(), tag2);
    tags.insert("tag3".to_owned(), tag3);
    tags.insert("tag4".to_owned(), tag4);
    let settings = Settings {
      workspace: "/test".to_owned(),
      default_after_workon: None,
      default_after_clone: None,
      default_tags: None,
      shell: None,
      tags: Some(tags),
    };
    Config {
      projects: projects,
      settings: settings,
    }
  }

  fn a_logger() -> Logger {
    use slog_term;
    use slog::{DrainExt, Level, LevelFilter};
    Logger::root(
      LevelFilter::new(
        slog_term::StreamerBuilder::new().stdout().build(),
        Level::Info,
      )
      .fuse(),
      o!(),
    )
  }
}

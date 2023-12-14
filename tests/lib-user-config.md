# User config

    >>> from gage._internal.user_config import *
    >>> from gage._internal.types import *

User config is non-project configuration provide by the user.

User config can be defined in one of two locations:

- With a project
- System wide (user directory)

User config defined for a project extends system wide user config.

    >>> from gage._internal.user_config import *

Create sample project config.

    >>> project_dir = make_temp_dir()
    >>> cd(project_dir)

    >>> write("gageconfig.toml", """
    ... [repos.git]
    ... type = "git"
    ... url = "git@github.com:gar1t/gage-runs.git"
    ...
    ... [repos.backup]
    ... path = "~/Backups/gage-runs"
    ... """)

Load the config.

    >>> config = user_config_for_dir(".")

    >>> config.as_json()  # +json +diff
    {
      "repos": {
        "backup": {
          "path": "~/Backups/gage-runs"
        },
        "git": {
          "type": "git",
          "url": "git@github.com:gar1t/gage-runs.git"
        }
      }
    }

    >>> validate_user_config_data(config.as_json())

`get_repositories()` returns a dict of repositories keyed by name.

    >>> repos = config.get_repositories()

    >>> sorted(repos)
    ['backup', 'git']

    >>> git_repo = repos["git"]
    >>> git_repo.as_json()  # +json +diff
    {
      "type": "git",
      "url": "git@github.com:gar1t/gage-runs.git"
    }

    >>> backup_repo = repos["backup"]
    >>> backup_repo.as_json()  # +json +diff
    {
      "path": "~/Backups/gage-runs"
    }


Each repository has a type.

    >>> git_repo.get_type()
    'git'

The default type is 'local'.

    >>> backup_repo.get_type()
    'local'

`attrs()` returns repository attributes.

    >>> git_repo.attrs()
    {'url': 'git@github.com:gar1t/gage-runs.git'}

    >>> backup_repo.attrs()
    {'path': '~/Backups/gage-runs'}

## Validation

    >>> from gage._internal.schema_util import validation_errors

    >>> def validate(data):
    ...     try:
    ...         validate_user_config_data(data)
    ...     except UserConfigValidationError as e:
    ...         json_pprint(validation_errors(e))

Empty config.

    >>> validate({})

    >>> validate({"repos": {}})

Unknown top-level attributes.

    >>> validate({"unknown": {}})
    [
      [
        "unknown"
      ]
    ]

Invalid repository type.

    >>> validate({"repos": 123})
    [
      "Properties ['repos'] are invalid",
      "The instance must be of type \"object\""
    ]

Invalid repo type.

    >>> validate({"repos": {"test": 123}})  # +wildcard
    [
      "Properties ['repos'] are invalid",
      "Properties ['test'] are invalid",
      "The instance must be of type \"object\"",
      ...
    ]

Missing required type or path.

    >>> validate({"repos": {"test": {}}})  # +wildcard
    [
      "Properties ['repos'] are invalid",
      "Properties ['test'] are invalid",
      ...
      "The object is missing required properties ['type']",
      "The object is missing required properties ['path']"
    ]

    >>> validate({"repos": {"test": {"foo": 123}}})  # +wildcard
    [
      "Properties ['repos'] are invalid",
      "Properties ['test'] are invalid",
      ...
      "The object is missing required properties ['type']",
      "The object is missing required properties ['path']"
    ]

Invalid type attribute.

    >>> validate({"repos": {"test": {"type": 123}}})
    [
      "Properties ['repos'] are invalid",
      "Properties ['test'] are invalid",
      "Properties ['type'] are invalid",
      "The instance must be of type \"string\""
    ]

Invalid path attribute.

    >>> validate({"repos": {"test": {"path": 123}}})
    [
      "Properties ['repos'] are invalid",
      "Properties ['test'] are invalid",
      "Properties ['path'] are invalid",
      "The instance must be of type \"string\""
    ]


## User config file locations

User config files may be defined for a project directory.
`user_config_candidates()` returns the list of names considered, in
order of precedence.

    >>> user_config_candidates()  # +pprint
    ['.gage/config.toml',
     '.gage/config.yaml',
     '.gage/config.json',
     'gageconfig.toml',
     'gageconfig.yaml',
     'gageconfig.json']

`user_config_path_for_dir()` locates a user config file within a
directory and returns its path.

Create a sample directory structure.

    >>> cd(make_temp_dir())

    >>> make_dir(".gage")
    >>> touch(".gage/config.toml")
    >>> touch(".gage/config.yaml")
    >>> touch(".gage/config.json")
    >>> touch("gageconfig.toml")
    >>> touch("gageconfig.yaml")
    >>> touch("gageconfig.json")

    >>> ls()
    .gage/config.json
    .gage/config.toml
    .gage/config.yaml
    gageconfig.json
    gageconfig.toml
    gageconfig.yaml

Apply `user_config_path_for_dir()` to the current directory.

    >>> user_config_path_for_dir(".")
    './.gage/config.toml'

    >>> rm(".gage/config.toml")

    >>> user_config_path_for_dir(".")
    './.gage/config.yaml'

    >>> rm(".gage/config.yaml")

    >>> user_config_path_for_dir(".")
    './.gage/config.json'

    >>> rm(".gage/config.json")

    >>> user_config_path_for_dir(".")
    './gageconfig.toml'

    >>> rm("./gageconfig.toml")

    >>> user_config_path_for_dir(".")
    './gageconfig.yaml'

    >>> rm("gageconfig.yaml")

    >>> user_config_path_for_dir(".")
    './gageconfig.json'

    >>> rm("gageconfig.json")

    >>> user_config_path_for_dir(".")
    Traceback (most recent call last):
    FileNotFoundError: .

## Combining project config and system config

Project config extends system config.

Create a sample project directory.

    >>> project_dir = make_temp_dir()

Create a sample user directory.

    >>> user_dir = make_temp_dir()

Function to return user config for the project directory given
the user directory.

    >>> def user_config():
    ...     with Env({"HOME": user_dir}):
    ...         return user_config_for_project(project_dir)

The user config is currently empty - there's nothing defined at the
project or system level.

    >>> user_config().as_json()  # +json
    {}

Create system config that defines a repo.

    >>> make_dir(path_join(user_dir, ".gage"))
    >>> write(path_join(user_dir, ".gage", "config.json"), """
    ... {
    ...   "repos": {
    ...     "backup": {
    ...       "path": "/Backup"
    ...     }
    ...   }
    ... }
    ... """)

    >>> user_config().as_json()  # +json
    {
      "repos": {
        "backup": {
          "path": "/Backup"
        }
      }
    }

Define a repo for the project.

    >>> write(path_join(project_dir, "gageconfig.yaml"), """
    ... repos:
    ...   remote:
    ...     type: git
    ...     url: git@github.com:xxx/yyy
    ... """)

    >>> user_config().as_json()  # +json
    {
      "repos": {
        "remote": {
          "type": "git",
          "url": "git@github.com:xxx/yyy"
        }
      }
    }

User config maintains references to parents. In this case, the project
level user config references the system level config.

    >>> user_config().parent.as_json()  # +json
    {
      "repos": {
        "backup": {
          "path": "/Backup"
        }
      }
    }

Parent configuration is included in the config interface.

Show the repositories defined in the user config.

    >>> repos = user_config().get_repositories()

    >>> sorted(repos.keys())
    ['backup', 'remote']

    >>> repos['backup'].as_json()  # +json
    {
      "path": "/Backup"
    }

    >>> repos['remote'].as_json()  # +json
    {
      "type": "git",
      "url": "git@github.com:xxx/yyy"
    }

Redefine the backup repo path in project level config.

    >>> write(path_join(project_dir, "gageconfig.yaml"), """
    ... repos:
    ...   remote:
    ...     type: git
    ...     url: git@github.com:xxx/yyy
    ...   backup:
    ...     path: .backup
    ... """)

    >>> user_config().get_repositories()['backup'].as_json()  # +json
    {
      "path": ".backup"
    }

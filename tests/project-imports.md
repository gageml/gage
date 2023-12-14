# Imports

    >>> import importlib
    >>> import gage

    >>> SKIP_DIRS = ["tests", "_vendor"]
    >>> SKIP_MODS = []

    >>> def iter_mods():
    ...     proj_root = os.path.dirname(gage.__file__)
    ...     for root, dirs, files in os.walk(proj_root, topdown=True):
    ...         for name in SKIP_DIRS:
    ...             if name in dirs: dirs.remove(name)
    ...         for name in files:
    ...             if not name.endswith(".py"): continue
    ...             mod_path = path_join(root, name)
    ...             mod_relpath = os.path.relpath(mod_path, proj_root)
    ...             mod_name = (
    ...                 "gage."
    ...                 + mod_relpath.replace(os.path.sep, ".")[:-3])
    ...             if mod_name in SKIP_MODS: continue
    ...             yield importlib.import_module(mod_name)

    >>> for name in sorted([m.__name__ for m in iter_mods()]):
    ...     print(name)  # +diff
    gage.__init__
    gage.__main__
    gage._internal.__init__
    gage._internal.ansi_util
    gage._internal.attr_log
    gage._internal.channel
    gage._internal.cli
    gage._internal.commands.associate
    gage._internal.commands.associate_impl
    gage._internal.commands.check
    gage._internal.commands.check_impl
    gage._internal.commands.comment
    gage._internal.commands.comment_impl
    gage._internal.commands.copy
    gage._internal.commands.copy_impl
    gage._internal.commands.delete
    gage._internal.commands.delete_impl
    gage._internal.commands.error_handlers
    gage._internal.commands.help
    gage._internal.commands.impl_support
    gage._internal.commands.label
    gage._internal.commands.label_impl
    gage._internal.commands.list
    gage._internal.commands.list_impl
    gage._internal.commands.main
    gage._internal.commands.main_impl
    gage._internal.commands.open
    gage._internal.commands.open_impl
    gage._internal.commands.operations
    gage._internal.commands.operations_impl
    gage._internal.commands.purge
    gage._internal.commands.purge_impl
    gage._internal.commands.restore
    gage._internal.commands.restore_impl
    gage._internal.commands.run
    gage._internal.commands.run_impl
    gage._internal.commands.select
    gage._internal.commands.select_impl
    gage._internal.commands.show
    gage._internal.commands.show_impl
    gage._internal.commands.sign
    gage._internal.exitcodes
    gage._internal.file_select
    gage._internal.file_util
    gage._internal.gagefile
    gage._internal.lang
    gage._internal.log
    gage._internal.opref_util
    gage._internal.project_util
    gage._internal.python_util
    gage._internal.repo
    gage._internal.repo_git
    gage._internal.repo_local
    gage._internal.repo_rclone
    gage._internal.repo_util
    gage._internal.run_comment
    gage._internal.run_config
    gage._internal.run_config_py
    gage._internal.run_context
    gage._internal.run_dependencies
    gage._internal.run_help
    gage._internal.run_output
    gage._internal.run_select
    gage._internal.run_sourcecode
    gage._internal.run_util
    gage._internal.schema_util
    gage._internal.shlex_util
    gage._internal.sys_config
    gage._internal.test
    gage._internal.typer_rich_util
    gage._internal.types
    gage._internal.user_config
    gage._internal.util
    gage._internal.var
    gage._internal.vcs_util
    gage._internal.yaml_util

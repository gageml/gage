# Command Impl Support

Command implementations are supported with `impl_support`.

   >>> from gage._internal.commands import impl_support

## Run Description

The function `_run_description` generates a description for a run.

This function uses a module level cache to avoid repeated project reads.

The cache is initially empty.

    >>> impl_support.__run_desc_field_name_cache
    {}

Generate a sample run.

    >>> use_example("hello")

    >>> run("gage run -qy")
    <0>

Get the run directory so we can use the run for tests.

    >>> run("gage select --meta-dir")  # +parse
    {meta_dir:path}
    <0>

Create a run for the director.

    >>> from gage._internal.run_util import run_for_meta_dir

    >>> sample_run = run_for_meta_dir(meta_dir)

Generate a description for the run.

    >>> impl_support._run_description(sample_run, 20)
    '[cyan1]name[/cyan1][bright_black]=[/bright_black][dim]Gage[/dim]'

Verify that the cache contains an entry for the sample run.

    >>> impl_support.__run_desc_field_name_cache  # +parse
    {'{cached_run_id:run_id}': []}

    >>> assert cached_run_id == sample_run.id

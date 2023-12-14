---
test-options: +skip (for CI until we get better control over tests)
---

# Sharing runs with others

In this scenario, a developer generates a run of interest and wants to
share it with a colleague. Motivations for sharing may include:

- Want to show a generated image along with inputs and logs
- Want to get feedback
- Want to get comments (additions to the run)

This scenario is where logging systems shine. Once logged to an
accessible location, any authorized user can easily see and comment on
logged results.

Gage is not a logging system, however. Results must be copied to a
location. Once copied, it can be viewed, either via a Web interface or
copied to another user's system. This is similar to Git.

There are various versions of "sharing":

1. Copy results to a remote repository for viewing via web UI
2. Copy results to a remote repository for retrieval and local viewing
3. Make a run available for viewing by hosting a Web UI and establishing
   network access to the local server
4. Make a local repository available as a repository to others
5. Share a screen (e.g. Zoom, Slack, etc.)

In option 1, a run is copied to a repository for remote viewing. The
server is configured both as a repository and as a hosted Gage Web UI. A
colleague requires both network access and authorization to view the
shared run.

In option 2, a run is copied to a remote repository where it is
available to authorized users for retrieval. Once retrieved to a local
system, the other user views the run.

Option 3 requires network routing to let users view a hosted Web UI on
another system. This is an esoteric scenario. It is not specifically
supported by Gage and requires network configuration and coordination.

In option 4, a user runs a local program (agent) to "serve" a local
repository to others. This communicate repository details without
copying files. This is also somewhat esoteric and is not supported yet.

Option 5 relies on simply sharing a screen using any variety of screen
sharing options. This scenario is always generally available to users
and is not covered here.

Of these options, the first is consistent with other experiment
tracking. This is essentially a logging scenario, where run details are
first written locally and then copied to a remote location for viewing.
This scenario is presented in detail in **Share and View** below.

The second option is presented in **Share and Retrieve**.

The other scenarios are not further covered.

## Share and View

In this scenario, local runs are copied to a location where they're
served/hosted with a Web UI. Authorized users view the runs from any
location that has network access to the hosted Web UI.

Generate a run to share.

    >>> use_example("hello")

    >>> run("gage run hello name=Sharon -l 'Something to share' -y")
    Hello Sharon
    <0>

    >>> run("gage ls -s")
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed | Something to share         |
    <0>

    >>> run("gage select --project-dir")  # +parse
    {project_dir:path}
    <0>

    >>> assert compare_paths(project_dir, ".")

Create a "remote location".

    >>> remote_runs = make_temp_dir()

    >>> run("gage ls -s", GAGE_RUNS=remote_runs)
    | # | operation | status | label                           |
    |---|-----------|--------|---------------------------------|
    <0>

Use `gage copy` to copy runs to the remote location.

    >>> run(f"gage copy --all --to '{remote_runs}' -y")
    Copied 1 run
    <0>

Compare local and remote files.

    >>> from gage._internal.sys_config import get_runs_home

    >>> local_runs = get_runs_home()

    >>> diffl(lsl(local_runs), lsl(remote_runs))  # +parse
    @@ -17,7 +17,6 @@
     {:run_id}.meta/started
     {:run_id}.meta/stopped
     {:run_id}.meta/sys/platform.json
    -{:run_id}.project
     {:run_id}.user/{:uuid4}.json
     {:run_id}/gage.toml
     {:run_id}/hello.py

Note that the files are the same except for the `.project` file, which
is not copied.

List remote runs.

    >>> run("gage ls -s", GAGE_RUNS=remote_runs)
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed | Something to share         |
    <0>

The remote run is not associated with a project.

    >>> run("gage select --project-dir", GAGE_RUNS=remote_runs)
    <0>

Once copied, the run is shared using the Gage Web UI.

This scenario forms the basis for "Share and Retrieve", which continues
below.

## Share and Retrieve

"Share and Retrieve" is a pattern where runs are copied (shared) to a
remote location and copied again (retrieved) from that location. This is
analogous to cloning Git repositories across systems: file are copied,
viewed and modified locally, and copied again as needed to share
changes.

The preceding section copies from a "local" location to a "remote"
location. This section continues with run retrieval.

Create a directory for a "retrieve" location.

    >>> retrieve_runs = make_temp_dir()

Copy runs from the remote location to the retrieve location.

    >>> run(f"gage copy --all --from '{remote_runs}' -y",
    ...     GAGE_RUNS=retrieve_runs)
    Copied runs
    <0>

Show retrieved runs.

    >>> run("gage ls -s", GAGE_RUNS=retrieve_runs)
    | # | operation   | status    | label                      |
    |---|-------------|-----------|----------------------------|
    | 1 | hello:hello | completed | Something to share         |
    <0>

Compare remote files and retrieved files.

    >>> diffl(lsl(remote_runs), lsl(retrieve_runs))

At this point the run is shared. The retrieve user can modify run user
attributes.

The retrieved run is not associated with a project.

    >>> run("gage select --project-dir", GAGE_RUNS=retrieve_runs)
    <0>

Associate the run with a project directory. This isn't necessary for the
"push and retrieve" pattern being demonstrated but is shows a typical
work flow. We show later that the project association on other systems
is not affected.

    >>> retrieve_project = make_temp_dir()

    >>> run(f"gage associate 1 '{retrieve_project}'",
    ...     GAGE_RUNS=retrieve_runs)  # +parse
    Associated "{:run_id}" with {x:path}
    <0>

    >>> assert x == retrieve_project

    >>> run("gage select --project-dir", GAGE_RUNS=retrieve_runs)
    ... # +parse
    {x:path}
    <0>

    >>> assert x == retrieve_project

Change the run label for the retrieved run.

    >>> run("gage label 1 --set 'Just an example' -y",
    ...     GAGE_RUNS=retrieve_runs)
    Set label for 1 run
    <0>

    >>> run("gage select --label", GAGE_RUNS=retrieve_runs)
    Just an example
    <0>

The local and remote runs aren't modified.

    >>> run("gage select --label")
    Something to share
    <0>

    >>> run("gage select --label", GAGE_RUNS=remote_runs)
    Something to share
    <0>

To share the new label, the retrieve copies the run to the remote
location.

    >>> run(f"gage copy 1 --to '{remote_runs}' -y",
    ...     GAGE_RUNS=retrieve_runs)
    Copied 1 run
    <0>

The project reference file is not copied to the remote location but the
new label logged attribute is.

    >>> diffl(lsl(retrieve_runs), lsl(remote_runs))  # +parse
    @@ -17,7 +17,6 @@
     {:run_id}.meta/started
     {:run_id}.meta/stopped
     {:run_id}.meta/sys/platform.json
    -{:run_id}.project
     {:run_id}.user/{:uuid4}.json
     {:run_id}.user/{:uuid4}.json
     {:run_id}/gage.toml

Show the label for the remote run.

    >>> run("gage select --label", GAGE_RUNS=remote_runs)
    Just an example
    <0>

Show the associated project for the remote run.

    >>> run("gage select --project-dir", GAGE_RUNS=remote_runs)
    <0>

When comparing the remote run to the local (original) run, we see a
missing project ref and a new logged attribute.

    >>> diffl(lsl(local_runs), lsl(remote_runs))  # +parse
    @@ -17,7 +17,7 @@
     {:run_id}.meta/started
     {:run_id}.meta/stopped
     {:run_id}.meta/sys/platform.json
    -{:run_id}.project
     {:run_id}.user/{:uuid4}.json
    +{:run_id}.user/{:uuid4}.json
     {:run_id}/gage.toml
     {:run_id}/hello.py

The original run is not yet affected.

    >>> run("gage select --label")
    Something to share
    <0>

Retrieve the remote runs for the local user.

    >>> run(f"gage copy --all --from '{remote_runs}' -y")
    Copied runs
    <0>

The local run now reflects the state shared by the retrieve user.

    >>> run("gage select --label")
    Just an example
    <0>

The local project association is unmodified.

    >>> run("gage select --project-dir")  # +parse
    {x:path}
    <0>

    >>> assert x == project_dir

    >>> diffl(lsl(remote_runs), lsl(local_runs))  # +parse
    @@ -17,6 +17,7 @@
     {:run_id}.meta/started
     {:run_id}.meta/stopped
     {:run_id}.meta/sys/platform.json
    +{:run_id}.project
     {:run_id}.user/{:uuid4}.json
     {:run_id}.user/{:uuid4}.json
     {:run_id}/gage.toml

## Notes for implementation

TODO

- Proper "copy from" interface: specify runs to copy, preview, progress,
  and summary message
- Options to copy only parts of run: meta, user, dir

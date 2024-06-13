# `run` command - batches

Gage will run a batch when `--batch` is specified. A batch may be
provided as a CSV or JSON file.

Use the `hello` example.

    >>> use_example("hello")

Create a batch for two runs in a CSV file.

    >>> tmp = make_temp_dir()

    >>> csv_filename = path_join(tmp, "batch-1.csv")
    >>> write(csv_filename, """
    ... name
    ... Joe
    ... Mike
    ... """.strip())

    >>> cat(csv_filename)
    name
    Joe
    Mike

Run the batch.

    >>> run(f"gage run --batch {csv_filename} -y")
    Hello Joe
    Hello Mike
    <0>

Create a JSON batch file.

    >>> json_filename = path_join(tmp, "batch-1.json")
    >>> json.dump(
    ...     [{"name": "Jane"}, {"name": "Robert"}],
    ...     open(json_filename, "w"),
    ...     indent=2, sort_keys=True,
    ... )

    >>> cat(json_filename)
    [
      {
        "name": "Jane"
      },
      {
        "name": "Robert"
      }
    ]

Run a batch with it.

    >>> run(f"gage run --batch {json_filename} -y")
    Hello Jane
    Hello Robert
    <0>

## Max Runs

The option `--max-runs` limits the size of a batch.

    >>> run(f"gage run hello -b {csv_filename} --max-runs 1 -y")
    Hello Joe
    <0>

    >>> run(f"gage run hello -b {csv_filename} --max-runs 0 -y")  # +parse
    gage: Nothing to run in {:path}.csv
    <1>

## Multiple Batch Files

Gage supports multiple `--batch` options. When a user specifies multiple
batch files, the batch is the cartesian product of the batch file items.

To illustrate, we create a sample project with an operation that adds
two numbers.

    >>> use_project(make_temp_dir())

    >>> write("add.py", """
    ... x = 1
    ... y = 2
    ... z = x + y
    ... print(f"{x} + {y} = {z}")
    ... """)

    >>> write("gage.toml", """
    ... [add]
    ... exec = "python add.py"
    ... config = "add.py"
    ... """)

Generate a test run.

    >>> run("gage run add x=2 y=3 -y")
    2 + 3 = 5
    <0>

Create two batch files, one for each config key (x and y).

    >>> write("x.csv", """
    ... x
    ... 1
    ... 2
    ... 3
    ... """.strip())

    >>> write("y.csv", """
    ... y
    ... 4
    ... 5
    ... """.strip())

Run a batch using the two batch files.

    >>> run("gage run add -b x.csv -b y.csv -y")
    1 + 4 = 5
    1 + 5 = 6
    2 + 4 = 6
    2 + 5 = 7
    3 + 4 = 7
    3 + 5 = 8
    <0>

## Batch Configuration

Batch files provide additional configuration to runs. Configuration
values that are not specified in batch files are read from the operation
defaults.

The `add` operation defined in the previous section has two
configuration keys: `x` and `y`, each with a default value.

    >>> run("gage run add -y")
    1 + 2 = 3
    <0>

The default values are used for config.

    >>> run("gage show --config")
    | x | 1 |
    | y | 2 |
    <0>

Run a batch using `y.csv`, which only specifies `y` values.

    >>> run("gage run add -b y.csv -y")
    1 + 4 = 5
    1 + 5 = 6
    <0>

    >>> run("gage show --config")
    | x | 1 |
    | y | 5 |
    <0>

Flags override default values.

    >>> run("gage run add -b y.csv x=11 -y")
    11 + 4 = 15
    11 + 5 = 16
    <0>

    >>> run("gage show --config")
    | x | 11 |
    | y | 5  |
    <0>

Flags override batch values as well.

    >>> run("gage run add -b y.csv y=22 -y")
    1 + 22 = 23
    1 + 22 = 23
    <0>

    >>> run("gage show --config")
    | x | 1  |
    | y | 22 |
    <0>

## Errors

Only CSV and JSON files are supported.

    >>> run("gage run add -b x.xyz -y")
    gage: Unsupported extension for batch file x.xyz
    <1>

Invalid JSON:

    >>> write("invalid.json", "[")

    >>> run("gage run add -b invalid.json -y")  # -space
    gage: Cannot read batch file invalid.json: Expecting value:
    line 1 column 2 (char 1)
    <1>

Missing batch files:

    >>> run("gage run add -b missing.csv -y")
    gage: Batch file missing.csv does not exist
    <1>

    >>> run("gage run add -b missing.json -y")
    gage: Batch file missing.json does not exist
    <1>

Batch does not currently support preview.

    >>> run("gage run add -b x.csv --preview")
    gage: Batch preview is not yet implemented
    <1>

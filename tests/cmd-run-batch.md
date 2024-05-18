# `run` command - batches

Gage will run a batch when `--batch` is specified. A batch may be
provided as a CSV or JSON file or the value "-", which indicates the
batch config is read from standard input.

Use the `hello` example.

    >>> use_example("hello")

Create a batch for two runs in a CSV file.

    >>> tmp = make_temp_dir()

    >>> csv_filename = path_join(tmp, "batch-1.csv")
    >>> write(csv_filename, """name
    ... joe
    ... mike
    ... """)

    >>> cat(csv_filename)
    name
    joe
    mike

Run the batch.

    >>> run(f"gage run --batch {csv_filename} -y")
    Hello joe
    Hello mike
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

# BATCHES

A *batch* is one or more runs started by `gage run` according to a
*batch file*.

A batch file may be one of the following formats:

- CSV (comma separated value)
- JSON

Batch files specify lists of configuration keys to use per run.

For CSV files, configuration keys are defined in the first row. Per run
values are defined on subsequent rows with each comma separated value
corresponding to a configuration key.

The following sample CSV file defines a batch of two runs with
configuration keys `height` and `width`:

```csv
height,width
10,10
20,20
```

JSON files define an array of object where each object is a set of
configuration keys per run.

Here is the equivalent of the previous example as JSON:

```json
[
  {
    "height": 10,
    "width": 10
  },
  {
    "height": 20,
    "width": 20
  }
]
```

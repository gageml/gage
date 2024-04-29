# GAGE FILE

## OVERVIEW

A Gage File specifies Gage ML operations. It may be defined using TOML,
JSON, or YAML and named with the corresponding file extension. For
example, **gage.json** is a JSON encoded Gage file.

Sample **gage.toml**

``` toml
[train]
exec = "python train.py"
```

Sample **gage.json**

```json
{
    "train": {
        "exec": "python train.py"
    }
}
```

Sample **gage.yaml**

``` yaml
train:
  exec: python train.py
```

## USING A GAGE FILE

Gage file are associated with *projects* that contain source code files.
To see which Gage file, if any, is applicable to Gage commands, run
'gage check -v'. If a Gage file exists in the current directory or one
of its parent directories, it is listed as **gagefile** in the output.

To list operations in the current Gage file, run 'gage operations'.

To run an operation, run 'gage run **OPERATION**'.

To show help for an operation, run 'gage run **OPERATION** --help-op'.

## VALIDATING

To validate a Gage file, run 'gage check **PATH**' where **PATH** is the
path to a Gage file.

## OPERATIONS

TODO

## NAMESPACES

TODO

## OPERATION DEFAULTS

TODO

## EXAMPLES

TODO

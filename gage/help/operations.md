# OPERATIONS

Operations specify how Gage ML runs something. Operations are defined in
a Gage file. For example, the following Gage file (written in TOML)
contains a single operation named **train** that is run by executing the
command 'python train.py'.

``` toml
[train]

exec = "python train.py"
```

Gage ML looks for a Gage file in the current directory or in a parent
directory. For more information run 'gage help gagefile'.

# Jupyter Notebooks Config

Jupyter Notebook config is similar to [Python
config](lib-run-config-py.md) but is applied to Python code cells
defined in the notebook.

    >>> from gage._internal.run_config_ipynb import *

The class `JupyterNotebookConfig` is used to parse notebook source and
apply configuration.

Use a sample Notebook to illustrate.

    >>> source = open(sample("scripts", "hello.ipynb")).read()

Initialize the config with the notebook source.

    >>> config = JupyterNotebookConfig(source)

    >>> config  # +json
    {
      "msg": "hello"
    }

Update the config with a new `msg`.

    >>> config["msg"] = "Hi ho"

Use `apply()` to apply the current configuration, returning the new
notebook source.

    >>> applied = config.apply()

Show the difference between the original source and the source with the
applied config.

    >>> diffs(source, applied)
    @@ -14,7 +14,7 @@
         }
        ],
        "source": [
    -    "msg = \"hello\"\n",
    +    "msg = \"Hi ho\"\n",
         "\n",
         "print(msg)"
        ]

## Magic Lines

IPython notebooks support magic lines, which start with `%s` in Python.

Gage follows the `nbconvert` logic and replaces magic lines with calls
to `get_ipython().run_line_magic()`.

The private function `_fix_magic_line` performs this replacement.

    >>> from gage._internal.run_config_ipynb import _fix_magic_line

    >>> _fix_magic_line("%")
    "get_ipython().run_line_magic('')\n"

    >>> _fix_magic_line("%foo")
    "get_ipython().run_line_magic('foo')\n"

    >>> _fix_magic_line("%foo bar")
    "get_ipython().run_line_magic('foo', 'bar')\n"

    >>> _fix_magic_line("%foo bar baz")
    "get_ipython().run_line_magic('foo', 'bar baz')\n"

The `magic.ipynb` sample notebook uses magic lines.

    >>> source = open(sample("scripts", "magic.ipynb")).read()

    >>> config = JupyterNotebookConfig(source)

    >>> config  # +json
    {
      "x": 1,
      "y": 2
    }

    >>> applied = config.apply()

    >>> diffs(source, applied)
    @@ -15,8 +15,8 @@
         }
        ],
        "source": [
    -    "%load_ext autoreload\n",
    -    "%autoreload 2\n",
    +    "get_ipython().run_line_magic('load_ext', 'autoreload')\n",
    +    "get_ipython().run_line_magic('autoreload', '2')\n",
         "\n",
         "x = 1\n",
         "y = 2\n",

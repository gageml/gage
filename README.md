# Gage ML

Gage ML is a tool for running model evaluations and publishing results.
It's under active development by the team that developed Guild AI.

## Installing

Gage ML is not released to PyPI. To install it, see **Setting Up a Local
Development Environment** below.

<!--

To install Gage ML, use `pip`.

``` shell
pip install gage
```

-->

## Contributing

We warmly encourage contributions to this project at any level. We are
committed to growing and supporting a rich, diverse set of contributors.
If you would like to contribute something, feel free to [open an
issue](https://github.com/gageml/gage/issues) to ask questions, make
suggestions, or otherwise let us know what you're thinking. Changes to
the project are accepted using GitHub pull requests.

Please read [Contributor Covenant Code of Conduct](CONTRIBUTING.md) to
familiarize yourself with the project code of conduct and contribution
policy. If you feel that something is missing from this document or
could be improved, please let us know by either opening an issue or by
emailing the project administrator at garrett@placeon.earth. Private
correspondence via email will be held in strict confidence.

### Setting up a Local Development Environment

1. For the source code repository at https://github.com/gage/gage

2. Create a project-specific virtual environment. **Use Python 3.10 or
   later.**

   ``` shell
   cd gage
   virtualenv --python python3.10 .venv  # Any version >= 3.10 is okay
   ```

3. Activate the virtual environment and use `pip` to install the project
   as "editable".

   ``` shell
   source .venv/bin/activate  # Works on most POSIX shells - change as needed
   pip install -e '.[dev]'    # Installs gage and its dev requirements
   ```

4. Run tests using [Groktest](https://github.com/gar1t/groktest)
   (optional).

   ``` shell
   groktest .
   ```

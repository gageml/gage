# Project version

The project version should be in sync with the `gage` package version.

    >>> import gage

    >>> import tomli
    >>> config = tomli.load(
    ...     open(path_join(gage.__pkgdir__, "pyproject.toml"), "rb")
    ... )

    >>> project_version = config["project"]["version"]
    >>> package_version = gage.__version__

    >>> assert project_version == package_version

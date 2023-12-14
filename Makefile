build:
	python -m build

install-dev:
	python -m pip install -e '.[dev]'

uninstall:
	python -m pip uninstall -y gage

wheel:
	python -m pip wheel --no-deps --wheel-dir dist .

clean:
	rm -rf dist build *.egg-info

venv: .venv/bin/activate

.venv/bin/activate:
	virtualenv --python python3.10 .venv

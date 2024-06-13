from pathlib import Path
import sys

path = sys.argv[1]
# Path(path).touch(mode=666)

with open(path, "w") as file:
    file.write("")

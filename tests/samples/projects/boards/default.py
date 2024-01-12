import json

foo = None
bar = None

with open("summary.json", "w") as f:
    json.dump(
        {
            "run": {"label": f"foo={foo} bar={bar}"},
            "attributes": {"foo": foo, "bar": bar},
        },
        f,
    )

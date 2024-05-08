import json

foo = None
bar = None

with open("summary.json", "w") as f:
    json.dump(
        {
            "attributes": {"foo": foo, "bar": bar},
        },
        f,
    )

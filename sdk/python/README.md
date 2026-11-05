# buwiz-computer Python SDK (workspace only)

Connect-only client. Not published to PyPI.

```python
import os
from buwiz_computer import BuwizComputer

box = BuwizComputer.connect(
    "http://127.0.0.1:1337",
    "http://127.0.0.1:1340",
    os.environ["BUWIZ_COMPUTER_TOKEN"],
)
box.exec(command=["echo", "ok"])
png = box.screenshot_png()
```

There is no `Sandbox.create()` / `docker run` helper. Start the guest yourself, then connect with the URLs you published. `/v1/info` endpoint URLs are ignored.

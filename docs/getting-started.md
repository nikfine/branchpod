# Getting started

Install `branchpod` first: see the [README](../README.md#installation). The command is `branchpod`. Messages and
the design document call it `bp`.

It does not create worktrees: use git, an IDE or any other tool for that, and have it call `up` and `down`.
Containers, dev servers and file sync are plain commands in the config; `branchpod` knows none of these tools by
name.

The example needs [Node.js](https://nodejs.org) 20.6 or newer. The commands are the same in every shell. Git
needs your `user.name` and `user.email`: without them the commit below fails, and `git worktree add` then makes an
empty worktree. Start with a new repository:

```
git init demo
cd demo
```

Create four files in it, saved as UTF-8 without a byte order mark (`>` in Windows PowerShell 5.1 writes UTF-16, and
with a byte order mark Node.js does not see the variable in `.env`). `branchpod.toml`:

```toml
version = 1

# A free port from this range, different in every worktree.
[port.web]
range = [8001, 8020]

# Git-ignored, so git does not bring it into a worktree: rendered from the one in the main checkout.
[[file]]
path = ".env"
mode = "template"
replace = [{ find = "DEMO_PORT=8000", with = "DEMO_PORT={{port.web}}" }]

# Anything that can be created and destroyed, such as a container. Here: a ref that remembers the commit
# the worktree had at `up`.
[[command]]
name = "base"
create = ["git", "update-ref", "refs/pods/{{pod.id}}", "HEAD"]
destroy = ["git", "-C", "{{main}}", "update-ref", "-d", "refs/pods/{{pod.id}}"]

# A process that runs until `down`.
[[service]]
name = "web"
run = ["node", "--env-file=.env", "server.js"]
```

`server.js`:

```js
const http = require("node:http");
const port = process.env.DEMO_PORT;
http
  .createServer((req, res) => res.end(`hello from port ${port}\n`))
  .listen(port, "127.0.0.1", () => console.log(`listening on http://127.0.0.1:${port}`));
```

`.env`:

```
DEMO_PORT=8000
```

`.gitignore`:

```
.env
```

Commit, create a worktree and bring it up:

```
git add .
git commit -m "Demo"
git worktree add ../demo-feature
branchpod up ../demo-feature
```

The id, the port and the pids will be different:

```
pod demo-feature-1a2b3c4d is up (/work/demo-feature)
  port       web                      8014
  file       .env                     template
  command    base                     (no output)
  supervisor supervisor               pid 4320
  service    web                      pid 4321
```

A *pod* is everything created for one worktree. Look at it, and at what the service printed. `status` ends with
`healthy`; the log may still be empty right after `up`:

```
branchpod status ../demo-feature
branchpod logs web ../demo-feature
```

Open the address from the log, then tear the pod down. Remove the worktree only after `down` printed `clean`:

```
branchpod down ../demo-feature
git worktree remove ../demo-feature
```

```
pod demo-feature-1a2b3c4d (/work/demo-feature)
  destroyed service    web
  destroyed supervisor supervisor
  destroyed command    base
  destroyed file       .env
  destroyed port       web
clean
```

What is left is the repository `demo` with a branch `demo-feature`: delete the directory when you are done.

Next: [commands and exit codes](commands.md), [configuration](config.md), [limitations](limitations.md).

# Hello Medha plugin

This package demonstrates all four current native component types together:
`greeting` skill, `/greet` action, `greetings` MCP server, and `welcome`
session-start hook. The two process components require Python 3 on `PATH`.
They make no network requests and ask for no filesystem or secret grants.

From the repository root:

```sh
medha plugins install examples/hello-plugin
medha plugins enable dev.medha.hello
medha plugins inspect dev.medha.hello
```

Start a session and use `/greet Ada`, or select the greeting skill. The MCP
server exposes one `hello` tool. Disable or remove it with `medha plugins
disable dev.medha.hello` or `medha plugins remove dev.medha.hello`.

# Security

Quarry opens database files and runs SQL against them, and DuckDB can read local files and,
with extensions, remote ones. Please report security problems privately, through
[GitHub's private vulnerability reporting](https://github.com/ggoggam/quarry/security/advisories/new),
not in a public issue. That includes a crafted database file that crashes or hangs Quarry, Quarry
running SQL the user didn't ask for, or credentials ending up in logs or files.

Include the Quarry version or commit, your OS, and the steps or a minimal file that reproduces it.
Never send a database that holds real personal or customer data.

Vulnerabilities in DuckDB itself go to [the DuckDB project](https://github.com/duckdb/duckdb/security).

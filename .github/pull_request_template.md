## Summary

<!-- What changed, and why. Link the issue it closes, if any: "Closes #12". -->

-

## Area

- [ ] Editor
- [ ] Result grid
- [ ] Schema browser
- [ ] Engine / driver (which: )
- [ ] Core (sessions, results, SQL splitting)
- [ ] Build, CI or release
- [ ] Docs only

## Validation

<!-- The commands you ran, and what you tried in the running app. -->

- [ ] `mise run pre-commit`
- [ ] `mise run test`
- [ ] Ran the app and tried the change (OS: )

```sh

```

## Checklist

- [ ] No blocking engine calls on the UI thread
- [ ] Large results stay lazy: nothing formats or copies every row
- [ ] No credentials, connection strings or private data in code, tests, commits or screenshots
- [ ] README updated if what Quarry supports changed

autopr
===

Automatically open topic pull requests from Anicca update notifications 

Dependencies
---

- Git
- [aosc-findupdate](https://github.com/AOSC-Dev/aosc-findupdate)

Deployment
---

The following variables must be set before running `autopr`:

- `autopr_webhook`: URL of the GitHub Webhook
- `bot_name`: Username of the autopr bot
- `github_token`: Personal access token of the GitHub identity
- `repo_url`: URL to the Anicca repository
- `update_list`: Path to the `update_list` file

The variables above can be stored in an `.env` file at the working directory.

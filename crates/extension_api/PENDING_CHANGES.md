# Pending Changes

This is a list of pending changes to the Zed extension API that require a breaking change.

This list should be updated as we notice things that should be changed so that we can batch them up in a single release.

## vNext

### Slash Commands

- Rename `SlashCommand.tooltip_text` to `SlashCommand.menu_text`
  - We may even want to remove it entirely, as right now this is only used for featured slash commands, and slash commands defined by extensions aren't currently able to be featured.

### Zed MD `0.9.0`

- `0.9.0` is a fork-only version for Zed MD's Markdown live preview hooks. It copies `0.8.0` and adds the `visual-md` interface, so its WIT will change until milestone M15 is complete, and extensions built for it must be rebuilt with each change.
  - It is never advertised to the extension registry, so it cannot collide with the `0.9.0` upstream will eventually release.
  - The `visual-md-apply-rule` export is the third hook after `visual-md-render-fence` and `visual-md-run-command`.
  - Extensions can make network requests with the `http-client` import without declaring a capability. This is the same as every earlier version.

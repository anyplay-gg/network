# Moderators are accounts with a role

People who moderate a cloud are ordinary accounts that the operator gave the moderator role. They sign in as they do every day and use the moderation paths with their own access token, so every action and every line of the audit log names the moderator. The operator's admin token stays for bootstrap: it names the first moderator and acts under the name `operator`, and an operator who never grants the role can still moderate alone. The statement of reasons, the appeal and the audit log are the same for both.

A public cloud has to tell a player why an action was taken, let a second person look at it when the player appeals, and show who did what. One shared token can do none of that: it has no name, it cannot be taken from one person without changing it for all, and it cannot keep the person who decided an appeal apart from the person who acted. The cost is that a stolen moderator session can act, so a moderator's token expires like any other and only the operator can grant or take back the role.

## Considered options

- One admin token for everything, as before: nothing to build, but no identity, no separation of duties and no way to log who acted.
- A token per moderator, issued by the operator: gives a name, but adds a second kind of credential to issue, rotate and revoke next to the sign-in that already exists.
- A separate staff directory or single sign-on: right for a large operator, too much for a volunteer cloud, and a second identity system to secure.

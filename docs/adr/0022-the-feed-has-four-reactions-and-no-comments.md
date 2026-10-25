# The feed has four reactions and no comments

A feed that friends answer feels alive. A feed with comments is a second message system: free text from anyone who sees an item, to be reported, reviewed and removed. Many players are teenagers ([ADR 0012](0012-private-by-default.md)), and the moderators are accounts, not a staff ([ADR 0015](0015-moderators-are-accounts.md)). A feed that cannot carry a sentence cannot carry abuse.

The feed answers with four fixed reactions: Nice, GG, Wow and Same. They are neutral, they mean something for each kind of item (a trophy, a new friend, a first game), and none says anything about a person. An account has one reaction per item and sees counts, not names. A player who wants to say more has messages ([ADR 0013](0013-messages-are-numbered-per-conversation.md)).

## Considered options

- Comments with the report flow of messages: the most expressive, and the largest moderation load, for text that every friend of the author can read.
- Emoji of the player's choice: free, but it is free text with extra steps, and some emoji are insults.
- No reactions: nothing to moderate, nothing to come back for.

## Consequences

- The reaction is an enum in the API (`Reaction` in `crates/api`); a value outside it is refused.
- Items hold numbers and kinds, never text a player wrote and never a name from a game (rule 1 of [Social](../SOCIAL.md)).
- Items and reactions expire after 90 days and go with the account.

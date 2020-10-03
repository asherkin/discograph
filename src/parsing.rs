use serenity::model::id::{ChannelId, UserId};

#[derive(Debug, Eq, PartialEq)]
pub enum Command {
    Help,
    Link,
    Graph(Option<ChannelId>),
    Stats,
    Dump, // TODO: Let this take a GuildId.
    Unknown(String),
}

impl Command {
    pub fn new_from_message(our_id: UserId, message: &str) -> Option<Command> {
        match internal::direct_mention_command(message, our_id.0) {
            Ok((_, "help")) => Some(Command::Help),
            Ok((_, "about")) => Some(Command::Help),
            Ok((_, "invite")) => Some(Command::Help),
            Ok((_, "link")) => Some(Command::Link),
            Ok((args, "graph")) => {
                let channel =
                    internal::channel_mention(args).map_or(None, |(_, id)| Some(ChannelId(id)));
                Some(Command::Graph(channel))
            }
            Ok((_, "stats")) => Some(Command::Stats),
            Ok((_, "dump")) => Some(Command::Dump),
            Ok((_, command)) => Some(Command::Unknown(command.to_string())),
            Err(_) => None,
        }
    }
}

pub fn parse_direct_mention(message: &str) -> Option<UserId> {
    match internal::direct_mention(message) {
        Ok((_, id)) => Some(UserId(id)),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_command() {
        let id = UserId(735929260073549854);
        let input = "wow, check out <@!735929260073549854>!";
        assert_eq!(Command::new_from_message(id, input), None);
    }

    #[test]
    fn basic() {
        let id = UserId(735929260073549854);
        let input = "<@!735929260073549854> stats";
        assert_eq!(Command::new_from_message(id, input), Some(Command::Stats));
    }

    #[test]
    fn quoted() {
        let id = UserId(735929260073549854);
        let input = "> hello\n<@!735929260073549854> stats";
        assert_eq!(Command::new_from_message(id, input), Some(Command::Stats));
    }

    #[test]
    fn unknown() {
        let id = UserId(735929260073549854);
        let input = "<@!735929260073549854> foobar";
        assert_eq!(
            Command::new_from_message(id, input),
            Some(Command::Unknown("foobar".to_string()))
        );
    }

    #[test]
    fn graph_channel() {
        let id = UserId(735929260073549854);
        let input = "<@!735929260073549854> graph <#335290997317697536>";
        assert_eq!(
            Command::new_from_message(id, input),
            Some(Command::Graph(Some(ChannelId(335290997317697536))))
        );
    }
}

pub(super) mod internal {
    use nom::bytes::complete::tag;
    use nom::character::complete::{alphanumeric0, digit1, line_ending, not_line_ending, space0};
    use nom::combinator::{all_consuming, map_res, opt, verify};
    use nom::multi::many0;
    use nom::sequence::tuple;
    use nom::IResult;

    fn parse_id(input: &str) -> IResult<&str, u64> {
        use std::str::FromStr;
        map_res(digit1, u64::from_str)(input)
    }

    fn user_mention(input: &str) -> IResult<&str, u64> {
        let mention_start = tag("<@");
        let nickname_mention = tag("!");
        let mention_end = tag(">");

        let (remaining, (_, _, id, _)) =
            tuple((mention_start, opt(nickname_mention), parse_id, mention_end))(input)?;
        Ok((remaining, id))
    }

    pub fn channel_mention(input: &str) -> IResult<&str, u64> {
        let mention_start = tag("<#");
        let mention_end = tag(">");

        let (remaining, (_, id, _)) = tuple((mention_start, parse_id, mention_end))(input)?;
        Ok((remaining, id))
    }

    fn consume_quote(input: &str) -> IResult<&str, ()> {
        let quote_start = tag("> ");

        let (remaining, _) = many0(tuple((quote_start, not_line_ending, line_ending)))(input)?;
        Ok((remaining, ()))
    }

    pub fn direct_mention(input: &str) -> IResult<&str, u64> {
        let (remaining, (_, id)) = tuple((consume_quote, user_mention))(input)?;
        Ok((remaining, id))
    }

    pub fn direct_mention_command(input: &str, wanted_id: u64) -> IResult<&str, &str> {
        let (_, (_, _, _, command, _, remaining)) = all_consuming(tuple((
            consume_quote,
            verify(user_mention, |id| *id == wanted_id),
            space0,
            alphanumeric0,
            space0,
            not_line_ending,
        )))(input)?;
        Ok((remaining, command))
    }

    mod test {
        #[test]
        fn user_mention() {
            let result = super::user_mention("<@735929260073549854>");
            assert_eq!(result, Ok(("", 735929260073549854u64)));
        }

        #[test]
        fn user_nickname_mention() {
            let result = super::user_mention("<@!735929260073549854>");
            assert_eq!(result, Ok(("", 735929260073549854u64)));
        }

        #[test]
        fn consume_quote() {
            let result = super::consume_quote("> foo\n> bar\nfoobar");
            assert_eq!(result, Ok(("foobar", ())));
        }

        #[test]
        fn simple_command() {
            let id = 735929260073549854u64;
            let result = super::direct_mention_command("<@!735929260073549854> cache", id);
            assert_eq!(result, Ok(("", "cache")));
        }

        #[test]
        fn simple_command_args() {
            let id = 735929260073549854u64;
            let result =
                super::direct_mention_command("<@!735929260073549854> cache status check", id);
            assert_eq!(result, Ok(("status check", "cache")));
        }

        #[test]
        fn reply_command() {
            let id = 735929260073549854u64;
            let result = super::direct_mention_command("> foo\n<@!735929260073549854> cache", id);
            assert_eq!(result, Ok(("", "cache")));
        }

        #[test]
        fn wrong_user() {
            let id = 735929260073549854u64;
            let result = super::direct_mention_command("<@!298220148647526402> hello!", id);
            assert!(result.is_err());
        }

        #[test]
        fn command_trailing_lines() {
            let id = 735929260073549854u64;
            let result = super::direct_mention_command("<@!735929260073549854> cache\nfoobar", id);
            assert!(result.is_err());
        }
    }
}

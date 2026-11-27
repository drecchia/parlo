//! Turns what an agent typed (a number or a chat id) into something the
//! backend accepts. Nothing here talks to WhatsApp: whether a number is
//! registered is the backend's `NewContact` check.

use crate::model::{Chat, ChatId};

/// Shortest and longest digit strings E.164 allows for a number with its
/// country code.
const DIGITS: std::ops::RangeInclusive<usize> = 7..=15;

/// Who a tool call is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A chat id exactly as the chat list shows it.
    Chat(ChatId),
    /// A phone number as digits with the country code.
    Phone(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetError {
    Missing,
    Both,
    BadPhone,
    BadChatId,
}

impl std::fmt::Display for TargetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Missing => "Give either chat_id or phone.",
            Self::Both => "Give chat_id or phone, not both.",
            Self::BadPhone => {
                "The phone number must be digits with the country code, such as 5562999999999 \
                 (7 to 15 digits; spaces, dashes, and a leading + are ignored)."
            }
            Self::BadChatId => {
                "The chat_id must look like 5562999999999@s.whatsapp.net or 1234567890@g.us, as \
                 whatsapp_list_chats returns it."
            }
        })
    }
}

/// Digits of a phone number, or `None` when it cannot be one.
pub fn normalize_phone(input: &str) -> Option<String> {
    let input = input.trim();
    let input = input.strip_prefix('+').unwrap_or(input);
    if input.is_empty()
        || !input
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ' ' | '-' | '(' | ')' | '.'))
    {
        return None;
    }
    let digits: String = input.chars().filter(char::is_ascii_digit).collect();
    // A leading zero is a national trunk prefix, not part of a number with a
    // country code.
    (DIGITS.contains(&digits.len()) && !digits.starts_with('0')).then_some(digits)
}

/// Servers a chat id may name.
const SERVERS: &[&str] = &["s.whatsapp.net", "g.us", "lid", "newsletter", "broadcast"];

/// Whether `id` has the shape of a chat id. A shape check only: the chat list
/// decides whether the chat exists.
pub fn looks_like_chat_id(id: &str) -> bool {
    let Some((user, server)) = id.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && user.len() <= 64
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        && SERVERS.contains(&server)
}

/// Picks the one target a call names.
pub fn parse_target(chat_id: Option<&str>, phone: Option<&str>) -> Result<Target, TargetError> {
    let chat_id = chat_id.map(str::trim).filter(|value| !value.is_empty());
    let phone = phone.map(str::trim).filter(|value| !value.is_empty());
    match (chat_id, phone) {
        (None, None) => Err(TargetError::Missing),
        (Some(_), Some(_)) => Err(TargetError::Both),
        (None, Some(phone)) => normalize_phone(phone)
            .map(Target::Phone)
            .ok_or(TargetError::BadPhone),
        (Some(id), None) => {
            if looks_like_chat_id(id) {
                Ok(Target::Chat(id.to_owned()))
            } else if id.chars().all(|c| c.is_ascii_digit() || c == '+')
                && let Some(digits) = normalize_phone(id)
            {
                // A bare number where a chat id was expected: be helpful.
                Ok(Target::Phone(digits))
            } else {
                Err(TargetError::BadChatId)
            }
        }
    }
}

/// The direct chat for `digits` among `chats`, if there is one. Chats behind
/// a privacy id are already filed under their number once it is known.
pub fn find_by_phone<'a>(chats: &'a [Chat], digits: &str) -> Option<&'a Chat> {
    chats.iter().find(|chat| chat.phone() == Some(digits))
}

/// The chat id the backend uses for a number.
pub fn direct_chat_id(digits: &str) -> ChatId {
    format!("{digits}@s.whatsapp.net")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_reduced_to_digits() {
        for (input, digits) in [
            ("5562999999999", "5562999999999"),
            ("+55 (62) 99999-9999", "5562999999999"),
            ("  +1.415.555.0100 ", "14155550100"),
        ] {
            assert_eq!(normalize_phone(input).as_deref(), Some(digits), "{input}");
        }
    }

    #[test]
    fn things_that_are_not_numbers_are_refused() {
        for input in [
            "",
            "+",
            "123456",
            "1234567890123456",
            "0062999999999",
            "55 62 abc",
            "5562999999999@s.whatsapp.net",
            "5562999999999; rm -rf /",
        ] {
            assert_eq!(normalize_phone(input), None, "{input}");
        }
    }

    #[test]
    fn chat_ids_need_a_known_server() {
        for id in [
            "5562999999999@s.whatsapp.net",
            "120363025246125486@g.us",
            "1234567890123@lid",
            "x-1_2.3:4@s.whatsapp.net",
        ] {
            assert!(looks_like_chat_id(id), "{id}");
        }
        for id in [
            "",
            "@g.us",
            "5562@example.com",
            "no at sign",
            "a b@g.us",
            "../x@g.us",
        ] {
            assert!(!looks_like_chat_id(id), "{id}");
        }
    }

    #[test]
    fn exactly_one_target_is_required() {
        assert_eq!(parse_target(None, None), Err(TargetError::Missing));
        assert_eq!(parse_target(Some(" "), Some("")), Err(TargetError::Missing));
        assert_eq!(
            parse_target(Some("1@g.us"), Some("5562999999999")),
            Err(TargetError::Both)
        );
        assert_eq!(parse_target(None, Some("12")), Err(TargetError::BadPhone));
        assert_eq!(
            parse_target(Some("nonsense"), None),
            Err(TargetError::BadChatId)
        );
        assert_eq!(
            parse_target(None, Some("+55 62 99999 9999")),
            Ok(Target::Phone("5562999999999".into()))
        );
        assert_eq!(
            parse_target(Some("1@g.us"), None),
            Ok(Target::Chat("1@g.us".into()))
        );
    }

    #[test]
    fn a_bare_number_in_chat_id_is_treated_as_a_phone() {
        assert_eq!(
            parse_target(Some("5562999999999"), None),
            Ok(Target::Phone("5562999999999".into()))
        );
    }

    #[test]
    fn known_chats_are_found_by_number() {
        let chats = vec![
            Chat::new("1203@g.us".into(), "Group".into()),
            Chat::new("5562999999999@s.whatsapp.net".into(), "Ana".into()),
        ];
        assert_eq!(
            find_by_phone(&chats, "5562999999999").map(|chat| chat.name.as_str()),
            Some("Ana")
        );
        assert!(find_by_phone(&chats, "5511988887777").is_none());
        assert_eq!(
            direct_chat_id("5562999999999"),
            "5562999999999@s.whatsapp.net"
        );
    }
}

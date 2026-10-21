//! Input forms for the team dialogs. Pure state: fields, which one the cursor sits on, and
//! validation. The networking happens in the caller, which receives the filled request.

/// Which dialog the form serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `sesync team join`: needs the server, the team, a member and machine name, and the
    /// 6-digit code from the authenticator that holds the team secret.
    Enroll,
    /// `sesync team create`: team name, optional member list, and the path to the admin token
    /// file. Never the token itself.
    Register,
    /// `sesync team invite`: the server, the team, the member to add, and the admin token file.
    Invite,
}

pub struct Field {
    pub name: &'static str,
    pub value: String,
    pub required: bool,
    /// 6 ascii digits, e.g. the TOTP code.
    pub digits: bool,
    /// An example drawn, dimmed, while the field is empty. Never a value: an empty field stays
    /// empty, so Enter on an untouched dialog is refused like any other empty field.
    pub example: &'static str,
    /// One line on what the field is for, shown while the cursor is on it.
    pub help: &'static str,
}

pub struct Form {
    pub kind: Kind,
    pub fields: Vec<Field>,
    pub cursor: usize,
}

impl Form {
    pub fn new(kind: Kind, prefill: &[(&str, &str)]) -> Form {
        let fields = match kind {
            Kind::Enroll => vec![
                ("server url"), ("team"), ("member"), ("machine"), ("code"),
            ],
            Kind::Register => vec![
                ("team"), ("members"), ("admin token file"), ("server url"),
            ],
            Kind::Invite => vec![
                ("server url"), ("team"), ("member"), ("admin token file"),
            ],
        };
        let mut form = Form {
            kind,
            fields: fields
                .into_iter()
                .map(|name| Form::field(name, kind))
                .collect(),
            cursor: 0,
        };
        for (name, value) in prefill {
            form.set_value(name, value);
        }
        form
    }

    fn field(name: &'static str, kind: Kind) -> Field {
        let (example, help) = Form::guide(name, kind);
        Field { name, value: String::new(), required: Form::required(name, kind), digits: name == "code", example, help }
    }

    /// (example, help) for a field. The examples are made-up names, so none of them is a
    /// usable default.
    fn guide(name: &str, kind: Kind) -> (&'static str, &'static str) {
        match (kind, name) {
            (_, "server url") => ("http://team-host:30500", "the team server's address"),
            (Kind::Register, "team") => ("design-team", "a name for the new team; members type it when they join"),
            (_, "team") => ("design-team", "the team's name, as the admin gave it"),
            (_, "members") => ("ana, rui", "optional: who may join, comma-separated; you too, to sync this machine"),
            (Kind::Invite, "member") => ("rui", "the person to add; they join under this name"),
            (_, "member") => ("ana", "your name, as the admin listed it"),
            (_, "machine") => ("ana-laptop", "a name for this machine, unique in the team"),
            (_, "code") => ("123456", "the 6 digits your authenticator app shows for this team"),
            (_, "admin token file") => ("~/.config/sesync/admin.token", "the file that holds the server's admin token, never the token itself"),
            _ => ("", ""),
        }
    }

    fn required(name: &str, kind: Kind) -> bool {
        match kind {
            Kind::Enroll => true,
            Kind::Register => name != "members", // members is optional
            Kind::Invite => true,
        }
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.fields.iter().map(|f| f.name).collect()
    }

    /// Every field as (label, current value) in dialog order.
    pub fn entries(&self) -> Vec<(&'static str, &str)> {
        self.fields.iter().map(|f| (f.name, f.value.as_str())).collect()
    }

    pub fn value(&self, name: &str) -> &str {
        self.fields.iter()
            .find(|f| f.name == name)
            .map(|f| f.value.as_str())
            .unwrap_or("")
    }

    pub fn set_value(&mut self, name: &str, value: &str) {
        if let Some(f) = self.fields.iter_mut().find(|f| f.name == name) {
            f.value = value.to_string();
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Move to the next field, wrapping around.
    pub fn next(&mut self) {
        self.cursor = (self.cursor + 1) % self.fields.len().max(1);
    }

    /// The field the cursor is on.
    pub fn current(&self) -> Option<&Field> {
        self.fields.get(self.cursor)
    }

    pub fn current_name(&self) -> &'static str {
        self.current().map_or("", |f| f.name)
    }

    pub fn push_char(&mut self, c: char) {
        if let Some(f) = self.fields.get_mut(self.cursor) {
            if f.digits {
                if c.is_ascii_digit() && f.value.len() < 6 {
                    f.value.push(c);
                }
            } else {
                f.value.push(c);
            }
        }
    }

    pub fn pop_char(&mut self) {
        if let Some(f) = self.fields.get_mut(self.cursor) {
            f.value.pop();
        }
    }

    /// Check every field; returns the first problem as something a human can act on.
    pub fn validate(&self) -> Result<(), String> {
        for f in &self.fields {
            if f.required && f.value.trim().is_empty() {
                return Err(format!("fill every field; '{}' is empty", f.name));
            }
            if f.digits && (f.value.len() != 6 || !f.value.chars().all(|c| c.is_ascii_digit())) {
                return Err("the code is 6 digits".into());
            }
        }
        Ok(())
    }

    /// (name, value) in field order, ready for the caller's network step.
    pub fn collected(&self) -> Vec<(String, String)> {
        // Trim every value: a trailing space typed into a field made the server URL invalid and
        // the dialog report "cannot reach the team server" while the server was up.
        self.fields.iter().map(|f| (f.name.to_string(), f.value.trim().to_string())).collect()
    }
}

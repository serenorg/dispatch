use dispatch_core::A2aAuthConfig;

pub(crate) fn format_a2a_auth_summary(auth: &A2aAuthConfig) -> String {
    match auth {
        A2aAuthConfig::Bearer { secret_name } => format!("auth=bearer:{secret_name}"),
        A2aAuthConfig::Header {
            header_name,
            secret_name,
        } => format!("auth=header:{header_name}({secret_name})"),
        A2aAuthConfig::Basic {
            username_secret_name,
            password_secret_name,
        } => format!("auth=basic:{username_secret_name}+{password_secret_name}"),
    }
}

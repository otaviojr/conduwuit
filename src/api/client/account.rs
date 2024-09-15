use std::fmt::Write;

use axum::extract::State;
use axum_client_ip::InsecureClientIp;
use conduit::{debug_info, error, info, utils, warn, Error, PduBuilder, Result};
use register::RegistrationKind;
use ruma::{
	api::client::{
		account::{
			change_password, check_registration_token_validity, deactivate, get_3pids, get_username_availability,
			register::{self, LoginType},
			request_3pid_management_token_via_email, request_3pid_management_token_via_msisdn, whoami,
			ThirdPartyIdRemovalStatus,
		},
		error::ErrorKind,
		uiaa::{AuthFlow, AuthType, UiaaInfo},
	},
	events::{
		room::{
			message::RoomMessageEventContent,
			power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
		},
		GlobalAccountDataEventType, StateEventType, TimelineEventType,
	},
	push, OwnedRoomId, UserId,
};
use serde_json::value::to_raw_value;
use service::Services;

use super::{join_room_by_id_helper, DEVICE_ID_LENGTH, SESSION_ID_LENGTH, TOKEN_LENGTH};
use crate::Ruma;

const RANDOM_USER_ID_LENGTH: usize = 10;

/// # `GET /_matrix/client/v3/register/available`
///
/// Checks if a username is valid and available on this server.
///
/// Conditions for returning true:
/// - The user id is not historical
/// - The server name of the user id matches this server
/// - No user or appservice on this server already claimed this username
///
/// Note: This will not reserve the username, so the username might become
/// invalid when trying to register
#[tracing::instrument(skip_all, fields(%client), name = "register_available")]
pub(crate) async fn get_register_available_route(
	State(services): State<crate::State>, InsecureClientIp(client): InsecureClientIp,
	body: Ruma<get_username_availability::v3::Request>,
) -> Result<get_username_availability::v3::Response> {
	// Validate user id
	let user_id = UserId::parse_with_server_name(body.username.to_lowercase(), services.globals.server_name())
		.ok()
		.filter(|user_id| !user_id.is_historical() && services.globals.user_is_local(user_id))
		.ok_or(Error::BadRequest(ErrorKind::InvalidUsername, "Username is invalid."))?;

	// Check if username is creative enough
	if services.users.exists(&user_id)? {
		return Err(Error::BadRequest(ErrorKind::UserInUse, "Desired user ID is already taken."));
	}

	if services
		.globals
		.forbidden_usernames()
		.is_match(user_id.localpart())
	{
		return Err(Error::BadRequest(ErrorKind::Unknown, "Username is forbidden."));
	}

	// TODO add check for appservice namespaces

	// If no if check is true we have an username that's available to be used.
	Ok(get_username_availability::v3::Response {
		available: true,
	})
}

/// # `POST /_matrix/client/v3/register`
///
/// Register an account on this homeserver.
///
/// You can use [`GET
/// /_matrix/client/v3/register/available`](fn.get_register_available_route.
/// html) to check if the user id is valid and available.
///
/// - Only works if registration is enabled
/// - If type is guest: ignores all parameters except
///   initial_device_display_name
/// - If sender is not appservice: Requires UIAA (but we only use a dummy stage)
/// - If type is not guest and no username is given: Always fails after UIAA
///   check
/// - Creates a new account and populates it with default account data
/// - If `inhibit_login` is false: Creates a device and returns device id and
///   access_token
#[allow(clippy::doc_markdown)]
#[tracing::instrument(skip_all, fields(%client), name = "register")]
pub(crate) async fn register_route(
	State(services): State<crate::State>, InsecureClientIp(client): InsecureClientIp, body: Ruma<register::v3::Request>,
) -> Result<register::v3::Response> {
	if !services.globals.allow_registration() && body.appservice_info.is_none() {
		info!(
			"Registration disabled and request not from known appservice, rejecting registration attempt for username \
			 {:?}",
			body.username
		);
		return Err(Error::BadRequest(ErrorKind::forbidden(), "Registration has been disabled."));
	}

	let is_guest = body.kind == RegistrationKind::Guest;

	if is_guest
		&& (!services.globals.allow_guest_registration()
			|| (services.globals.allow_registration() && services.globals.config.registration_token.is_some()))
	{
		info!(
			"Guest registration disabled / registration enabled with token configured, rejecting guest registration \
			 attempt, initial device name: {:?}",
			body.initial_device_display_name
		);
		return Err(Error::BadRequest(
			ErrorKind::GuestAccessForbidden,
			"Guest registration is disabled.",
		));
	}

	// forbid guests from registering if there is not a real admin user yet. give
	// generic user error.
	if is_guest && services.users.count()? < 2 {
		warn!(
			"Guest account attempted to register before a real admin user has been registered, rejecting \
			 registration. Guest's initial device name: {:?}",
			body.initial_device_display_name
		);
		return Err(Error::BadRequest(ErrorKind::forbidden(), "Registration temporarily disabled."));
	}

	let user_id = match (&body.username, is_guest) {
		(Some(username), false) => {
			let proposed_user_id =
				UserId::parse_with_server_name(username.to_lowercase(), services.globals.server_name())
					.ok()
					.filter(|user_id| !user_id.is_historical() && services.globals.user_is_local(user_id))
					.ok_or(Error::BadRequest(ErrorKind::InvalidUsername, "Username is invalid."))?;

			if services.users.exists(&proposed_user_id)? {
				return Err(Error::BadRequest(ErrorKind::UserInUse, "Desired user ID is already taken."));
			}

			if services
				.globals
				.forbidden_usernames()
				.is_match(proposed_user_id.localpart())
			{
				return Err(Error::BadRequest(ErrorKind::Unknown, "Username is forbidden."));
			}

			proposed_user_id
		},
		_ => loop {
			let proposed_user_id = UserId::parse_with_server_name(
				utils::random_string(RANDOM_USER_ID_LENGTH).to_lowercase(),
				services.globals.server_name(),
			)
			.unwrap();
			if !services.users.exists(&proposed_user_id)? {
				break proposed_user_id;
			}
		},
	};

	if body.body.login_type == Some(LoginType::ApplicationService) {
		if let Some(ref info) = body.appservice_info {
			if !info.is_user_match(&user_id) {
				return Err(Error::BadRequest(ErrorKind::Exclusive, "User is not in namespace."));
			}
		} else {
			return Err(Error::BadRequest(ErrorKind::MissingToken, "Missing appservice token."));
		}
	} else if services.appservice.is_exclusive_user_id(&user_id).await {
		return Err(Error::BadRequest(ErrorKind::Exclusive, "User ID reserved by appservice."));
	}

	// UIAA
	let mut uiaainfo;
	let skip_auth = if services.globals.config.registration_token.is_some() {
		// Registration token required
		uiaainfo = UiaaInfo {
			flows: vec![AuthFlow {
				stages: vec![AuthType::RegistrationToken],
			}],
			completed: Vec::new(),
			params: Box::default(),
			session: None,
			auth_error: None,
		};
		body.appservice_info.is_some()
	} else {
		// No registration token necessary, but clients must still go through the flow
		uiaainfo = UiaaInfo {
			flows: vec![AuthFlow {
				stages: vec![AuthType::Dummy],
			}],
			completed: Vec::new(),
			params: Box::default(),
			session: None,
			auth_error: None,
		};
		body.appservice_info.is_some() || is_guest
	};

	if !skip_auth {
		if let Some(auth) = &body.auth {
			let (worked, uiaainfo) = services.uiaa.try_auth(
				&UserId::parse_with_server_name("", services.globals.server_name()).expect("we know this is valid"),
				"".into(),
				auth,
				&uiaainfo,
			)?;
			if !worked {
				return Err(Error::Uiaa(uiaainfo));
			}
		// Success!
		} else if let Some(json) = body.json_body {
			uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
			services.uiaa.create(
				&UserId::parse_with_server_name("", services.globals.server_name()).expect("we know this is valid"),
				"".into(),
				&uiaainfo,
				&json,
			)?;
			return Err(Error::Uiaa(uiaainfo));
		} else {
			return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
		}
	}

	let password = if is_guest {
		None
	} else {
		body.password.as_deref()
	};

	// Create user
	services.users.create(&user_id, password)?;

	// Default to pretty displayname
	let mut displayname = user_id.localpart().to_owned();

	// If `new_user_displayname_suffix` is set, registration will push whatever
	// content is set to the user's display name with a space before it
	if !services.globals.new_user_displayname_suffix().is_empty() && body.appservice_info.is_none() {
		write!(displayname, " {}", services.globals.config.new_user_displayname_suffix)
			.expect("should be able to write to string buffer");
	}

	services
		.users
		.set_displayname(&user_id, Some(displayname.clone()))
		.await?;

	// Initial account data
	services.account_data.update(
		None,
		&user_id,
		GlobalAccountDataEventType::PushRules.to_string().into(),
		&serde_json::to_value(ruma::events::push_rules::PushRulesEvent {
			content: ruma::events::push_rules::PushRulesEventContent {
				global: push::Ruleset::server_default(&user_id),
			},
		})
		.expect("to json always works"),
	)?;

	// Inhibit login does not work for guests
	if !is_guest && body.inhibit_login {
		return Ok(register::v3::Response {
			access_token: None,
			user_id,
			device_id: None,
			refresh_token: None,
			expires_in: None,
		});
	}

	// Generate new device id if the user didn't specify one
	let device_id = if is_guest {
		None
	} else {
		body.device_id.clone()
	}
	.unwrap_or_else(|| utils::random_string(DEVICE_ID_LENGTH).into());

	// Generate new token for the device
	let token = utils::random_string(TOKEN_LENGTH);

	// Create device for this account
	services.users.create_device(
		&user_id,
		&device_id,
		&token,
		body.initial_device_display_name.clone(),
		Some(client.to_string()),
	)?;

	debug_info!(%user_id, %device_id, "User account was created");

	let device_display_name = body.initial_device_display_name.clone().unwrap_or_default();

	// log in conduit admin channel if a non-guest user registered
	if body.appservice_info.is_none() && !is_guest {
		if !device_display_name.is_empty() {
			info!("New user \"{user_id}\" registered on this server with device display name: {device_display_name}");
			services
				.admin
				.send_message(RoomMessageEventContent::notice_plain(format!(
					"New user \"{user_id}\" registered on this server from IP {client} and device display name \
					 \"{device_display_name}\""
				)))
				.await;
		} else {
			info!("New user \"{user_id}\" registered on this server.");
			services
				.admin
				.send_message(RoomMessageEventContent::notice_plain(format!(
					"New user \"{user_id}\" registered on this server from IP {client}"
				)))
				.await;
		}
	}

	// log in conduit admin channel if a guest registered
	if body.appservice_info.is_none() && is_guest && services.globals.log_guest_registrations() {
		info!("New guest user \"{user_id}\" registered on this server.");

		if !device_display_name.is_empty() {
			services
				.admin
				.send_message(RoomMessageEventContent::notice_plain(format!(
					"Guest user \"{user_id}\" with device display name \"{device_display_name}\" registered on this \
					 server from IP {client}"
				)))
				.await;
		} else {
			services
				.admin
				.send_message(RoomMessageEventContent::notice_plain(format!(
					"Guest user \"{user_id}\" with no device display name registered on this server from IP {client}",
				)))
				.await;
		}
	}

	// If this is the first real user, grant them admin privileges except for guest
	// users Note: the server user, @conduit:servername, is generated first
	if !is_guest {
		if let Some(admin_room) = services.admin.get_admin_room()? {
			if services.rooms.state_cache.room_joined_count(&admin_room)? == Some(1) {
				services.admin.make_user_admin(&user_id).await?;

				warn!("Granting {user_id} admin privileges as the first user");
			}
		}
	}

	if body.appservice_info.is_none()
		&& !services.globals.config.auto_join_rooms.is_empty()
		&& (services.globals.allow_guests_auto_join_rooms() || !is_guest)
	{
		for room in &services.globals.config.auto_join_rooms {
			if !services
				.rooms
				.state_cache
				.server_in_room(services.globals.server_name(), room)?
			{
				warn!("Skipping room {room} to automatically join as we have never joined before.");
				continue;
			}

			if let Some(room_id_server_name) = room.server_name() {
				if let Err(e) = join_room_by_id_helper(
					&services,
					&user_id,
					room,
					Some("Automatically joining this room upon registration".to_owned()),
					&[room_id_server_name.to_owned(), services.globals.server_name().to_owned()],
					None,
					&body.appservice_info,
				)
				.await
				{
					// don't return this error so we don't fail registrations
					error!("Failed to automatically join room {room} for user {user_id}: {e}");
				} else {
					info!("Automatically joined room {room} for user {user_id}");
				};
			}
		}
	}

	Ok(register::v3::Response {
		access_token: Some(token),
		user_id,
		device_id: Some(device_id),
		refresh_token: None,
		expires_in: None,
	})
}

/// # `POST /_matrix/client/r0/account/password`
///
/// Changes the password of this account.
///
/// - Requires UIAA to verify user password
/// - Changes the password of the sender user
/// - The password hash is calculated using argon2 with 32 character salt, the
///   plain password is
/// not saved
///
/// If logout_devices is true it does the following for each device except the
/// sender device:
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
#[tracing::instrument(skip_all, fields(%client), name = "change_password")]
pub(crate) async fn change_password_route(
	State(services): State<crate::State>, InsecureClientIp(client): InsecureClientIp,
	body: Ruma<change_password::v3::Request>,
) -> Result<change_password::v3::Response> {
	// Authentication for this endpoint was made optional, but we need
	// authentication currently
	let sender_user = body
		.sender_user
		.as_ref()
		.ok_or_else(|| Error::BadRequest(ErrorKind::MissingToken, "Missing access token."))?;
	let sender_device = body.sender_device.as_ref().expect("user is authenticated");

	let mut uiaainfo = UiaaInfo {
		flows: vec![AuthFlow {
			stages: vec![AuthType::Password],
		}],
		completed: Vec::new(),
		params: Box::default(),
		session: None,
		auth_error: None,
	};

	if let Some(auth) = &body.auth {
		let (worked, uiaainfo) = services
			.uiaa
			.try_auth(sender_user, sender_device, auth, &uiaainfo)?;
		if !worked {
			return Err(Error::Uiaa(uiaainfo));
		}
	// Success!
	} else if let Some(json) = body.json_body {
		uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
		services
			.uiaa
			.create(sender_user, sender_device, &uiaainfo, &json)?;
		return Err(Error::Uiaa(uiaainfo));
	} else {
		return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
	}

	services
		.users
		.set_password(sender_user, Some(&body.new_password))?;

	if body.logout_devices {
		// Logout all devices except the current one
		for id in services
			.users
			.all_device_ids(sender_user)
			.filter_map(Result::ok)
			.filter(|id| id != sender_device)
		{
			services.users.remove_device(sender_user, &id)?;
		}
	}

	info!("User {sender_user} changed their password.");
	services
		.admin
		.send_message(RoomMessageEventContent::notice_plain(format!(
			"User {sender_user} changed their password."
		)))
		.await;

	Ok(change_password::v3::Response {})
}

/// # `GET _matrix/client/r0/account/whoami`
///
/// Get `user_id` of the sender user.
///
/// Note: Also works for Application Services
pub(crate) async fn whoami_route(
	State(services): State<crate::State>, body: Ruma<whoami::v3::Request>,
) -> Result<whoami::v3::Response> {
	let sender_user = body.sender_user.as_ref().expect("user is authenticated");
	let device_id = body.sender_device.clone();

	Ok(whoami::v3::Response {
		user_id: sender_user.clone(),
		device_id,
		is_guest: services.users.is_deactivated(sender_user)? && body.appservice_info.is_none(),
	})
}

/// # `POST /_matrix/client/r0/account/deactivate`
///
/// Deactivate sender user account.
///
/// - Leaves all rooms and rejects all invitations
/// - Invalidates all access tokens
/// - Deletes all device metadata (device id, device display name, last seen ip,
///   last seen ts)
/// - Forgets all to-device events
/// - Triggers device list updates
/// - Removes ability to log in again
#[tracing::instrument(skip_all, fields(%client), name = "deactivate")]
pub(crate) async fn deactivate_route(
	State(services): State<crate::State>, InsecureClientIp(client): InsecureClientIp,
	body: Ruma<deactivate::v3::Request>,
) -> Result<deactivate::v3::Response> {
	// Authentication for this endpoint was made optional, but we need
	// authentication currently
	let sender_user = body
		.sender_user
		.as_ref()
		.ok_or_else(|| Error::BadRequest(ErrorKind::MissingToken, "Missing access token."))?;
	let sender_device = body.sender_device.as_ref().expect("user is authenticated");

	let mut uiaainfo = UiaaInfo {
		flows: vec![AuthFlow {
			stages: vec![AuthType::Password],
		}],
		completed: Vec::new(),
		params: Box::default(),
		session: None,
		auth_error: None,
	};

	if let Some(auth) = &body.auth {
		let (worked, uiaainfo) = services
			.uiaa
			.try_auth(sender_user, sender_device, auth, &uiaainfo)?;
		if !worked {
			return Err(Error::Uiaa(uiaainfo));
		}
	// Success!
	} else if let Some(json) = body.json_body {
		uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
		services
			.uiaa
			.create(sender_user, sender_device, &uiaainfo, &json)?;
		return Err(Error::Uiaa(uiaainfo));
	} else {
		return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
	}

	// Remove profile pictures and display name
	let all_joined_rooms: Vec<OwnedRoomId> = services
		.rooms
		.state_cache
		.rooms_joined(sender_user)
		.filter_map(Result::ok)
		.collect();

	full_user_deactivate(&services, sender_user, all_joined_rooms).await?;

	info!("User {sender_user} deactivated their account.");
	services
		.admin
		.send_message(RoomMessageEventContent::notice_plain(format!(
			"User {sender_user} deactivated their account."
		)))
		.await;

	Ok(deactivate::v3::Response {
		id_server_unbind_result: ThirdPartyIdRemovalStatus::NoSupport,
	})
}

/// # `GET _matrix/client/v3/account/3pid`
///
/// Get a list of third party identifiers associated with this account.
///
/// - Currently always returns empty list
pub(crate) async fn third_party_route(body: Ruma<get_3pids::v3::Request>) -> Result<get_3pids::v3::Response> {
	let _sender_user = body.sender_user.as_ref().expect("user is authenticated");

	Ok(get_3pids::v3::Response::new(Vec::new()))
}

/// # `POST /_matrix/client/v3/account/3pid/email/requestToken`
///
/// "This API should be used to request validation tokens when adding an email
/// address to an account"
///
/// - 403 signals that The homeserver does not allow the third party identifier
///   as a contact option.
pub(crate) async fn request_3pid_management_token_via_email_route(
	_body: Ruma<request_3pid_management_token_via_email::v3::Request>,
) -> Result<request_3pid_management_token_via_email::v3::Response> {
	Err(Error::BadRequest(
		ErrorKind::ThreepidDenied,
		"Third party identifier is not allowed",
	))
}

/// # `POST /_matrix/client/v3/account/3pid/msisdn/requestToken`
///
/// "This API should be used to request validation tokens when adding an phone
/// number to an account"
///
/// - 403 signals that The homeserver does not allow the third party identifier
///   as a contact option.
pub(crate) async fn request_3pid_management_token_via_msisdn_route(
	_body: Ruma<request_3pid_management_token_via_msisdn::v3::Request>,
) -> Result<request_3pid_management_token_via_msisdn::v3::Response> {
	Err(Error::BadRequest(
		ErrorKind::ThreepidDenied,
		"Third party identifier is not allowed",
	))
}

/// # `GET /_matrix/client/v1/register/m.login.registration_token/validity`
///
/// Checks if the provided registration token is valid at the time of checking
///
/// Currently does not have any ratelimiting, and this isn't very practical as
/// there is only one registration token allowed.
pub(crate) async fn check_registration_token_validity(
	State(services): State<crate::State>, body: Ruma<check_registration_token_validity::v1::Request>,
) -> Result<check_registration_token_validity::v1::Response> {
	let Some(reg_token) = services.globals.config.registration_token.clone() else {
		return Err(Error::BadRequest(
			ErrorKind::forbidden(),
			"Server does not allow token registration.",
		));
	};

	Ok(check_registration_token_validity::v1::Response {
		valid: reg_token == body.token,
	})
}

/// Runs through all the deactivation steps:
///
/// - Mark as deactivated
/// - Removing display name
/// - Removing avatar URL and blurhash
/// - Removing all profile data
/// - Leaving all rooms (and forgets all of them)
pub async fn full_user_deactivate(
	services: &Services, user_id: &UserId, all_joined_rooms: Vec<OwnedRoomId>,
) -> Result<()> {
	services.users.deactivate_account(user_id)?;

	super::update_displayname(services, user_id, None, all_joined_rooms.clone()).await?;
	super::update_avatar_url(services, user_id, None, None, all_joined_rooms.clone()).await?;

	let all_profile_keys = services
		.users
		.all_profile_keys(user_id)
		.filter_map(Result::ok);

	for (profile_key, _profile_value) in all_profile_keys {
		if let Err(e) = services.users.set_profile_key(user_id, &profile_key, None) {
			warn!("Failed removing {user_id} profile key {profile_key}: {e}");
		}
	}

	for room_id in all_joined_rooms {
		let state_lock = services.rooms.state.mutex.lock(&room_id).await;

		let room_power_levels = services
			.rooms
			.state_accessor
			.room_state_get(&room_id, &StateEventType::RoomPowerLevels, "")?
			.as_ref()
			.and_then(|event| serde_json::from_str(event.content.get()).ok()?)
			.and_then(|content: RoomPowerLevelsEventContent| content.into());

		let user_can_demote_self = room_power_levels
			.as_ref()
			.is_some_and(|power_levels_content| {
				RoomPowerLevels::from(power_levels_content.clone()).user_can_change_user_power_level(user_id, user_id)
			}) || services
			.rooms
			.state_accessor
			.room_state_get(&room_id, &StateEventType::RoomCreate, "")?
			.as_ref()
			.is_some_and(|event| event.sender == user_id);

		if user_can_demote_self {
			let mut power_levels_content = room_power_levels.unwrap_or_default();
			power_levels_content.users.remove(user_id);

			// ignore errors so deactivation doesn't fail
			if let Err(e) = services
				.rooms
				.timeline
				.build_and_append_pdu(
					PduBuilder {
						event_type: TimelineEventType::RoomPowerLevels,
						content: to_raw_value(&power_levels_content).expect("event is valid, we just created it"),
						unsigned: None,
						state_key: Some(String::new()),
						redacts: None,
						timestamp: None,
					},
					user_id,
					&room_id,
					&state_lock,
				)
				.await
			{
				warn!(%room_id, %user_id, "Failed to demote user's own power level: {e}");
			} else {
				info!("Demoted {user_id} in {room_id} as part of account deactivation");
			}
		}
	}

	super::leave_all_rooms(services, user_id).await;

	Ok(())
}

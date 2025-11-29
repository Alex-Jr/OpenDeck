use super::{GenericInstancePayload, send_to_plugin};

use crate::events::frontend::instances::{key_moved, update_state};
use crate::shared::{ActionContext, Context};
use crate::store::profiles::{acquire_locks_mut, get_slot_mut, save_profile};

use tokio::time::sleep;

use std::time::Duration;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::LazyLock;

use serde::Serialize;

#[derive(Serialize)]
struct KeyEvent {
	event: &'static str,
	action: String,
	context: ActionContext,
	device: String,
	payload: GenericInstancePayload,
}

struct KeyState {
	count: u8,
}

static KEY_STATES: LazyLock<Mutex<HashMap<(String, u8), KeyState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub async fn key_down(device: &str, key: u8) -> Result<(), anyhow::Error> {
	let mut locks = acquire_locks_mut().await;
	let selected_profile = locks.device_stores.get_selected_profile(device)?;
	let context = Context {
		device: device.to_owned(),
		profile: selected_profile.to_owned(),
		controller: "Keypad".to_owned(),
		position: key,
	};

	let _ = key_moved(crate::APP_HANDLE.get().unwrap(), context.clone(), true).await;

	let Some(instance) = get_slot_mut(&context, &mut locks).await? else { return Ok(()) };
	if instance.action.uuid == "opendeck.multiaction" {
		println!("multi action");
		for child in instance.children.as_mut().unwrap() {
			send_to_plugin(
				&child.action.plugin,
				&KeyEvent {
					event: "keyDown",
					action: child.action.uuid.clone(),
					context: child.context.clone(),
					device: child.context.device.clone(),
					payload: GenericInstancePayload::new(child),
				},
			)
			.await?;

			tokio::time::sleep(Duration::from_millis(100)).await;

			if child.states.len() == 2 && !child.action.disable_automatic_states {
				child.current_state = (child.current_state + 1) % (child.states.len() as u16);
			}

			send_to_plugin(
				&child.action.plugin,
				&KeyEvent {
					event: "keyUp",
					action: child.action.uuid.clone(),
					context: child.context.clone(),
					device: child.context.device.clone(),
					payload: GenericInstancePayload::new(child),
				},
			)
			.await?;

			tokio::time::sleep(Duration::from_millis(100)).await;
		}

		let contexts = instance.children.as_ref().unwrap().iter().map(|x| x.context.clone()).collect::<Vec<_>>();
		for child in contexts {
			let _ = update_state(crate::APP_HANDLE.get().unwrap(), child, &mut locks).await;
		}

		save_profile(device, &mut locks).await?;
	} else if instance.action.uuid == "opendeck.toggleaction" {
		let children = instance.children.as_ref().unwrap();
		if children.is_empty() {
			return Ok(());
		}
		let child = &children[instance.current_state as usize];
		send_to_plugin(
			&child.action.plugin,
			&KeyEvent {
				event: "keyDown",
				action: child.action.uuid.clone(),
				context: child.context.clone(),
				device: child.context.device.clone(),
				payload: GenericInstancePayload::new(child),
			},
		)
		.await?;
	} else if instance.action.uuid == "opendeck.doubletap" {
		let children = instance.children.as_ref().unwrap(); // clone para mover para spawn
		if children.is_empty() {
			return Ok(());
		}

		let children_clone = children.clone();

		let id = (device.to_string(), key);

		let mut states = KEY_STATES.lock().unwrap();

		if let Some(state) = states.get_mut(&id) {
			state.count += 1;
			return Ok(());
		}

		states.insert(id.clone(), KeyState {
			count: 1,
		});

		drop(states);

		tokio::spawn(async move {
			sleep(Duration::from_millis(200)).await;

			let action_to_run = {
				let mut states = KEY_STATES.lock().unwrap();

				if let Some(state) = states.remove(&id) {
					if state.count == 1 {
						"single"
					} else {
						"double"
					}
				} else {
					return;
				}
			};

			match action_to_run {
				"single" => {
					println!("single tap");
					let child = &children_clone[0];
					if children_clone.is_empty() {
						return;
					}

					let _ = send_to_plugin(
						&child.action.plugin,
						&KeyEvent {
							event: "keyDown",
							action: child.action.uuid.clone(),
							context: child.context.clone(),
							device: child.context.device.clone(),
							payload: GenericInstancePayload::new(child),
						},
					).await;
				}
				"double" => {
					println!("double tap");
					if children_clone.len() < 2 {
						return;
					}
					let child = &children_clone[1];

					let _ = send_to_plugin(
						&child.action.plugin,
						&KeyEvent {
							event: "keyDown",
							action: child.action.uuid.clone(),
							context: child.context.clone(),
							device: child.context.device.clone(),
							payload: GenericInstancePayload::new(child),
						},
					).await;
				}
				_ => {}
			}
		});
	} else {
		send_to_plugin(
			&instance.action.plugin,
			&KeyEvent {
				event: "keyDown",
				action: instance.action.uuid.clone(),
				context: instance.context.clone(),
				device: instance.context.device.clone(),
				payload: GenericInstancePayload::new(instance),
			},
		)
		.await?;
	}

	Ok(())
}

pub async fn key_up(device: &str, key: u8) -> Result<(), anyhow::Error> {
	let mut locks = acquire_locks_mut().await;
	let selected_profile = locks.device_stores.get_selected_profile(device)?;
	let context = Context {
		device: device.to_owned(),
		profile: selected_profile.to_owned(),
		controller: "Keypad".to_owned(),
		position: key,
	};

	let _ = key_moved(crate::APP_HANDLE.get().unwrap(), context.clone(), false).await;

	let slot = get_slot_mut(&context, &mut locks).await?;
	let Some(instance) = slot else { return Ok(()) };

	if instance.action.uuid == "opendeck.toggleaction" {
		let index = instance.current_state as usize;
		let children = instance.children.as_ref().unwrap();
		if children.is_empty() {
			return Ok(());
		}
		let child = &children[index];
		send_to_plugin(
			&child.action.plugin,
			&KeyEvent {
				event: "keyUp",
				action: child.action.uuid.clone(),
				context: child.context.clone(),
				device: child.context.device.clone(),
				payload: GenericInstancePayload::new(child),
			},
		)
		.await?;
		instance.current_state = ((index + 1) % instance.children.as_ref().unwrap().len()) as u16;
	} else if instance.action.uuid != "opendeck.multiaction" {
		if instance.states.len() == 2 && !instance.action.disable_automatic_states {
			instance.current_state = (instance.current_state + 1) % (instance.states.len() as u16);
		}
		send_to_plugin(
			&instance.action.plugin,
			&KeyEvent {
				event: "keyUp",
				action: instance.action.uuid.clone(),
				context: instance.context.clone(),
				device: instance.context.device.clone(),
				payload: GenericInstancePayload::new(instance),
			},
		)
		.await?;
	};

	let _ = update_state(crate::APP_HANDLE.get().unwrap(), instance.context.clone(), &mut locks).await;
	save_profile(device, &mut locks).await?;

	Ok(())
}

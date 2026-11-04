use super::Error;

use crate::shared::{Action, ActionContext, ActionInstance, ActionState, Context, config_dir};
use crate::store::profiles::{LocksMut, acquire_locks, acquire_locks_mut, get_instance_mut, get_slot, get_slot_mut, save_profile_now};

use tauri::{AppHandle, Emitter, Manager, command};
use tokio::fs::remove_dir_all;

#[command]
pub async fn create_instance(app: AppHandle, mut action: Action, context: Context) -> Result<Option<ActionInstance>, Error> {
	if !action.controllers.contains(&context.controller) {
		return Ok(None);
	}

	if context.controller == "Encoder" {
		let _ = crate::shared::initialise_encoder_layout(&mut action, None);
	}

	let mut locks = acquire_locks_mut().await;
	let slot = get_slot_mut(&context, &mut locks).await?;

	if let Some(parent) = slot {
		let Some(children) = &mut parent.children else { return Ok(None) };
		let index = match children.last() {
			None => 1,
			Some(instance) => instance.context.index + 1,
		};

		let instance = ActionInstance {
			action: action.clone(),
			context: ActionContext::from_context(context.clone(), index),
			states: action.states.clone(),
			current_state: 0,
			settings: serde_json::Value::Object(serde_json::Map::new()),
			children: None,
		};
		children.push(instance.clone());

		if parent.action.uuid == "opendeck.toggleaction" && parent.states.len() < children.len() {
			parent.states.push(crate::shared::ActionState {
				image: "opendeck/toggle-action.png".to_owned(),
				..Default::default()
			});
			let _ = update_state(&app, parent.context.clone(), &mut locks).await;
		}

		save_profile_now(&context.device, &mut locks).await?;
		drop(locks);
		let _ = crate::events::outbound::will_appear::will_appear(&instance).await;

		let locks = acquire_locks().await;
		let slot = get_slot(&context, &locks).await?.clone();
		Ok(slot)
	} else {
		let instance = ActionInstance {
			action: action.clone(),
			context: ActionContext::from_context(context.clone(), 0),
			states: action.states.clone(),
			current_state: 0,
			settings: serde_json::Value::Object(serde_json::Map::new()),
			children: if matches!(action.uuid.as_str(), "opendeck.multiaction" | "opendeck.toggleaction") {
				Some(vec![])
			} else {
				None
			},
		};

		*slot = Some(instance.clone());
		let slot = slot.clone();

		save_profile_now(&context.device, &mut locks).await?;
		let _ = crate::events::outbound::will_appear::will_appear(&instance).await;

		Ok(slot)
	}
}

fn instance_images_dir(context: &ActionContext) -> std::path::PathBuf {
	config_dir()
		.join("images")
		.join(&context.device)
		.join(&context.profile)
		.join(format!("{}.{}.{}", context.controller, context.position, context.index))
}

// Embed position-owned images as data URLs so saving writes them at the new position.
fn relocate_instance(instance: &mut ActionInstance, destination: &Context) -> Result<(), std::io::Error> {
	use base64::Engine;
	let images_dir = instance_images_dir(&instance.context);
	for state in &mut instance.states {
		for image in std::iter::once(&mut state.image).chain(state.image_override.iter_mut()) {
			let path = std::path::Path::new(image);
			if path.starts_with(&images_dir) {
				let extension = path.extension().and_then(|ext| ext.to_str()).unwrap_or("png");
				let mime = if extension == "svg" { "svg+xml" } else { extension };
				*image = format!("data:image/{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(std::fs::read(path)?));
			}
		}
	}
	instance.context = ActionContext::from_context(destination.clone(), instance.context.index);
	if let Some(children) = &mut instance.children {
		for child in children {
			relocate_instance(child, destination)?;
		}
	}
	Ok(())
}

#[command]
pub async fn move_instance(source: Context, destination: Context, retain: bool) -> Result<Option<(ActionInstance, Option<ActionInstance>)>, Error> {
	if source.controller != destination.controller || source == destination {
		return Ok(None);
	}

	let mut locks = acquire_locks_mut().await;
	// Handles swapping instances between two positions
	if get_slot_mut(&destination, &mut locks).await?.is_some() {
		if retain || source.device != destination.device || source.profile != destination.profile {
			return Ok(None);
		}
		let Some(mut first) = get_slot_mut(&source, &mut locks).await?.clone() else { return Ok(None) };
		let Some(mut second) = get_slot_mut(&destination, &mut locks).await?.clone() else {
			return Ok(None);
		};
		let old_first = first.clone();
		let old_second = second.clone();
		relocate_instance(&mut first, &destination)?;
		relocate_instance(&mut second, &source)?;
		// Notify plugins that the old contexts disappear before registering the new ones.
		for instance in [&old_first, &old_second] {
			for action in std::iter::once(instance).chain(instance.children.iter().flatten()) {
				let _ = crate::events::outbound::will_appear::will_disappear(action, false).await;
			}
		}
		*get_slot_mut(&source, &mut locks).await? = Some(second.clone());
		*get_slot_mut(&destination, &mut locks).await? = Some(first.clone());
		save_profile_now(&source.device, &mut locks).await?;
		for instance in [&first, &second] {
			for action in std::iter::once(instance).chain(instance.children.iter().flatten()) {
				let _ = crate::events::outbound::will_appear::will_appear(action).await;
			}
		}
		return Ok(Some((first, Some(second))));
	}

	// Handles moving an instance to a new position, optionally retaining the original
	let src = get_slot_mut(&source, &mut locks).await?;

	let Some(mut new) = src.clone() else {
		return Ok(None);
	};
	new.context = ActionContext::from_context(destination.clone(), 0);
	if let Some(children) = &mut new.children {
		for (index, instance) in children.iter_mut().enumerate() {
			instance.context = ActionContext::from_context(destination.clone(), index as u16 + 1);
			for (i, state) in instance.states.iter_mut().enumerate() {
				if !instance.action.states[i].image.is_empty() {
					state.image = instance.action.states[i].image.clone();
				} else {
					state.image = instance.action.icon.clone();
				}
			}
		}
	}

	let old_dir = instance_images_dir(&src.as_ref().unwrap().context);
	let new_dir = instance_images_dir(&new.context);
	let _ = tokio::fs::create_dir_all(&new_dir).await;
	if let Ok(files) = old_dir.read_dir() {
		for file in files.flatten() {
			let _ = tokio::fs::copy(file.path(), new_dir.join(file.file_name())).await;
		}
	}
	for state in new.states.iter_mut() {
		let path = std::path::Path::new(&state.image);
		if path.starts_with(&old_dir) {
			state.image = new_dir.join(path.strip_prefix(&old_dir).unwrap()).to_string_lossy().into_owned();
		}

		if let Some(image_override) = state.image_override.as_mut() {
			let override_path = std::path::Path::new(image_override);
			if override_path.starts_with(&old_dir) {
				*image_override = new_dir.join(override_path.strip_prefix(&old_dir).unwrap()).to_string_lossy().into_owned();
			}
		}
	}

	let dst = get_slot_mut(&destination, &mut locks).await?;
	*dst = Some(new.clone());

	if !retain {
		let src = get_slot_mut(&source, &mut locks).await?;
		if let Some(old) = src {
			let _ = crate::events::outbound::will_appear::will_disappear(old, true).await;
			let _ = remove_dir_all(instance_images_dir(&old.context)).await;
		}
		*src = None;
	}

	let _ = crate::events::outbound::will_appear::will_appear(&new).await;

	save_profile_now(&destination.device, &mut locks).await?;

	Ok(Some((new, None)))
}

#[command]
pub async fn remove_instance(context: ActionContext) -> Result<(), Error> {
	let mut locks = acquire_locks_mut().await;
	let slot = get_slot_mut(&(&context).into(), &mut locks).await?;
	let Some(instance) = slot else {
		return Ok(());
	};

	if instance.context == context {
		let _ = crate::events::outbound::will_appear::will_disappear(instance, true).await;
		if let Some(children) = &instance.children {
			for child in children {
				let _ = crate::events::outbound::will_appear::will_disappear(child, true).await;
				let _ = remove_dir_all(instance_images_dir(&child.context)).await;
			}
		}
		let _ = remove_dir_all(instance_images_dir(&instance.context)).await;
		*slot = None;
	} else {
		let children = instance.children.as_mut().unwrap();
		for (index, child) in children.iter().enumerate() {
			if child.context == context {
				let _ = crate::events::outbound::will_appear::will_disappear(child, true).await;
				let _ = remove_dir_all(instance_images_dir(&child.context)).await;
				children.remove(index);

				if instance.action.uuid == "opendeck.multiaction"
					&& let Some(settings) = instance.settings.as_object_mut()
					&& let Some(delays) = settings.get_mut("delays").and_then(|v| v.as_array_mut())
				{
					if index == 0 {
						if !delays.is_empty() {
							delays.remove(0);
						}
					} else if index - 1 < delays.len() {
						delays.remove(index - 1);
					}
				}

				break;
			}
		}
		if instance.action.uuid == "opendeck.toggleaction" {
			if instance.current_state as usize >= children.len() {
				instance.current_state = if children.is_empty() { 0 } else { children.len() as u16 - 1 };
			}
			if !children.is_empty() {
				instance.states.pop();
				let _ = update_state(crate::APP_HANDLE.get().unwrap(), instance.context.clone(), &mut locks).await;
			}
		}
	}

	save_profile_now(&context.device, &mut locks).await?;

	Ok(())
}

#[derive(Clone, serde::Serialize)]
struct UpdateStateEvent {
	context: ActionContext,
	contents: Option<ActionInstance>,
}

pub async fn update_state(app: &AppHandle, context: ActionContext, locks: &mut LocksMut<'_>) -> Result<(), anyhow::Error> {
	let window = app.get_webview_window("main").unwrap();
	window.emit(
		"update_state",
		UpdateStateEvent {
			contents: get_instance_mut(&context, locks).await?.cloned(),
			context,
		},
	)?;
	Ok(())
}

#[command]
pub async fn set_state(context: ActionContext, index: u16, state: ActionState) -> Result<(), Error> {
	let mut locks = acquire_locks_mut().await;
	let reference = get_instance_mut(&context, &mut locks).await?.unwrap();
	reference.states[index as usize] = state;
	let clone = reference.clone();
	save_profile_now(&context.device, &mut locks).await?;
	crate::events::outbound::states::title_parameters_did_change(&clone, index).await?;
	Ok(())
}

#[command]
pub async fn set_child_delay(parent_context: ActionContext, index: usize, delay_ms: u64) -> Result<serde_json::Value, Error> {
	let mut locks = acquire_locks_mut().await;
	let Some(parent) = get_instance_mut(&parent_context, &mut locks).await? else {
		return Ok(serde_json::Value::Null);
	};

	let delays = parent.settings.get_mut("delays").and_then(|v| v.as_array_mut());
	if let Some(arr) = delays {
		if arr.len() <= index {
			arr.resize(index + 1, serde_json::json!(100));
		}
		arr[index] = serde_json::json!(delay_ms);
	} else {
		if !parent.settings.is_object() {
			parent.settings = serde_json::Value::Object(serde_json::Map::new());
		}
		let map = parent.settings.as_object_mut().unwrap();
		let mut arr = vec![serde_json::json!(100); index + 1];
		arr[index] = serde_json::json!(delay_ms);
		map.insert("delays".to_string(), serde_json::Value::Array(arr));
	}
	let parent_settings = parent.settings.clone();

	save_profile_now(&parent_context.device, &mut locks).await?;
	Ok(parent_settings)
}

#[command]
pub async fn update_image(context: Context, image: Option<String>) {
	if Some(&context.profile) != crate::store::profiles::DEVICE_STORES.write().await.get_selected_profile(&context.device).ok().as_ref() {
		return;
	}

	if let Err(error) = crate::events::outbound::devices::update_image(context, image).await {
		log::warn!("Failed to update device image: {}", error);
	}
}

#[command]
pub async fn trigger_virtual_press(context: Context) -> Result<(), Error> {
	let event = || crate::events::inbound::PayloadEvent {
		payload: crate::events::inbound::devices::PressPayload {
			device: context.device.clone(),
			position: context.position,
		},
	};
	match context.controller.as_str() {
		"Keypad" => {
			crate::events::inbound::devices::key_down(event()).await?;
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			crate::events::inbound::devices::key_up(event()).await?;
		}
		"Encoder" => {
			crate::events::inbound::devices::encoder_down(event()).await?;
			tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			crate::events::inbound::devices::encoder_up(event()).await?;
		}
		_ => {}
	}

	Ok(())
}

#[derive(Clone, serde::Serialize)]
struct KeyMovedEvent {
	context: Context,
	pressed: bool,
}

pub async fn key_moved(app: &AppHandle, context: Context, pressed: bool) -> Result<(), anyhow::Error> {
	let window = app.get_webview_window("main").unwrap();
	window.emit("key_moved", KeyMovedEvent { context, pressed })?;
	Ok(())
}

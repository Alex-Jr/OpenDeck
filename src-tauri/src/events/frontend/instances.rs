use super::Error;

use std::path::Path;

use crate::shared::{Action, ActionContext, ActionInstance, Context, config_dir};
use crate::store::profiles::{LocksMut, acquire_locks_mut, get_instance_mut, get_slot_mut, save_profile};

use tauri::{AppHandle, Emitter, Manager, command};
use tokio::fs::remove_dir_all;

#[command]
pub async fn create_instance(app: AppHandle, action: Action, context: Context) -> Result<Option<ActionInstance>, Error> {
	if !action.controllers.contains(&context.controller) {
		return Ok(None);
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

		save_profile(&context.device, &mut locks).await?;
		let _ = crate::events::outbound::will_appear::will_appear(&instance).await;

		Ok(Some(instance))
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

		save_profile(&context.device, &mut locks).await?;
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

fn update_children_and_states(instance: &mut ActionInstance, base_context: &Context, old_dir: &Path, new_dir: &Path) {
    instance.context = ActionContext::from_context(base_context.clone(), 0);

	if let Some(children) = &mut instance.children {
        for (index, child) in children.iter_mut().enumerate() {
            child.context = ActionContext::from_context(base_context.clone(), index as u16 + 1);
            for (i, state) in child.states.iter_mut().enumerate() {
                state.image = if !child.action.states[i].image.is_empty() {
                    child.action.states[i].image.clone()
                } else {
                    child.action.icon.clone()
                };
            }
        }
    }

	for state in instance.states.iter_mut() {
        let path = Path::new(&state.image);

        if let Ok(rel) = path.strip_prefix(old_dir) {
            state.image = new_dir.join(rel).to_string_lossy().into_owned();
        }
    }
}


#[derive(Clone, serde::Serialize)]
pub struct MoveInstanceResponse {
	moved_instance: ActionInstance,
	replaced_instance: Option<ActionInstance>,
}

#[command]
pub async fn move_instance(source: Context, destination: Context, retain: bool) -> Result<Option<MoveInstanceResponse>, Error> {
    if source.controller != destination.controller || (
		source.position == destination.position && 
		source.profile == destination.profile && 
		source.device == destination.device
	) {
        return Ok(None);
    }

    let mut locks = acquire_locks_mut().await;
    
	let mut src_instance = {
		let src_slot: &mut Option<ActionInstance> = get_slot_mut(&source, &mut locks).await?;

		let instance = if retain {
			src_slot.clone()
		} else {
			src_slot.take()
		};

		match instance {
			Some(i) => { i },
			_ => return Ok(None),
		}
	};

	let mut dst_instance = {
		let dst_slot: &mut Option<ActionInstance> = get_slot_mut(&destination, &mut locks).await?;

		dst_slot.clone()
	};

	let src_dir = instance_images_dir(&ActionContext::from_context(source.clone(), 0));
	let dst_dir = instance_images_dir(&ActionContext::from_context(destination.clone(), 0));

	let had_old = src_dir.exists();
	let had_new = dst_dir.exists();

	let tmp_dir = dst_dir.with_file_name("tmp_swap_dir");
	let _ = tokio::fs::create_dir_all(&dst_dir).await;
	let _ = tokio::fs::create_dir_all(&src_dir).await;
	let _ = tokio::fs::create_dir_all(&tmp_dir).await;
	let _ = tokio::fs::rename(&dst_dir, &tmp_dir).await;
	let _ = tokio::fs::rename(&src_dir, &dst_dir).await;
	let _ = tokio::fs::rename(&tmp_dir, &src_dir).await;
	let _ = remove_dir_all(&tmp_dir).await;

	if !had_old {
		let _ = remove_dir_all(&dst_dir).await;
	}

	if !had_new {
		let _ = remove_dir_all(&src_dir).await;
	}
	
	if !retain {
		let _ = crate::events::outbound::will_appear::will_disappear(&src_instance, true).await;
	}
    update_children_and_states(&mut src_instance, &destination, &src_dir, &dst_dir);
	let dst_slot = get_slot_mut(&destination, &mut locks).await?;
	*dst_slot = Some(src_instance.clone());
	let _ = crate::events::outbound::will_appear::will_appear(&src_instance).await;

	if let Some(ref mut dst_instance) = dst_instance {
		let _ = crate::events::outbound::will_appear::will_disappear(dst_instance, true).await;
		update_children_and_states(dst_instance, &source, &dst_dir, &src_dir);
		let src_slot = get_slot_mut(&source, &mut locks).await?;
		*src_slot = Some(dst_instance.clone());
		let _ = crate::events::outbound::will_appear::will_appear(dst_instance).await;
	}

    save_profile(&destination.device, &mut locks).await?;
	
    Ok(Some(MoveInstanceResponse {
		moved_instance: src_instance,
		replaced_instance: dst_instance,
	}))
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
		for (index, instance) in children.iter().enumerate() {
			if instance.context == context {
				let _ = crate::events::outbound::will_appear::will_disappear(instance, true).await;
				let _ = remove_dir_all(instance_images_dir(&instance.context)).await;
				children.remove(index);
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

	save_profile(&context.device, &mut locks).await?;

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
pub async fn set_state(instance: ActionInstance, state: u16) -> Result<(), Error> {
	let mut locks = acquire_locks_mut().await;
	let reference = get_instance_mut(&instance.context, &mut locks).await?.unwrap();
	*reference = instance.clone();
	save_profile(&instance.context.device, &mut locks).await?;
	crate::events::outbound::states::title_parameters_did_change(&instance, state).await?;
	Ok(())
}

#[command]
pub async fn update_image(context: Context, image: String) {
	if Some(&context.profile) != crate::store::profiles::DEVICE_STORES.write().await.get_selected_profile(&context.device).ok().as_ref() {
		return;
	}

	if let Err(error) = crate::events::outbound::devices::update_image(context, Some(image)).await {
		log::warn!("Failed to update device image: {}", error);
	}
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

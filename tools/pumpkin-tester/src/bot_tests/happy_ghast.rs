use std::sync::{Arc, atomic::Ordering};

use pumpkin_core::{
    entity::{Entity, EntityBase, passive::happy_ghast::HappyGhastEntity, player::Player},
    server::Server,
};
use pumpkin_data::{
    data_component_impl::EquipmentSlot, entity::EntityType, item::Item, item_stack::ItemStack,
    packet::CURRENT_MC_VERSION,
};
use pumpkin_protocol::{
    ClientPacket, RawPacket,
    codec::var_int::VarInt,
    java::server::play::{SConfirmTeleport, SMoveVehicle, SPlayerInput, SPlayerPosition},
};
use pumpkin_util::math::vector3::Vector3;

pub async fn run_happy_ghast_dismount_test(
    server: &Arc<Server>,
    summary_lines: &mut Vec<String>,
) -> Result<(), String> {
    let player = server
        .get_player_by_name("TestBot1")
        .ok_or("Missing TestBot1")?;
    let world = player.world();
    let original_pos = player.position();
    let player_chunk = player.get_entity().chunk_pos.load();
    let spawn_pos = Vector3::new(
        f64::from(player_chunk.x << 4) + 8.0,
        original_pos.y + 6.0,
        f64::from(player_chunk.y << 4) + 8.0,
    );
    let ghast = HappyGhastEntity::new(Entity::new(
        world.clone(),
        spawn_pos,
        &EntityType::HAPPY_GHAST,
    ));
    let ghast_base = ghast.get_entity();
    ghast.mob_entity.set_no_ai(true);
    ghast_base.set_has_no_gravity(true);
    ghast.mob_entity.set_item_slot(
        &EquipmentSlot::BODY,
        ItemStack::new(1, &Item::WHITE_HARNESS),
    );
    world
        .level
        .get_or_fetch_chunk(ghast_base.chunk_pos.load(), |_| ())
        .await;
    world.update_active_chunks();
    if !world.spawn_entity(ghast.clone()) {
        return Err("Could not spawn the Happy Ghast".into());
    }

    let result = if ghast_base.add_passenger(ghast.clone(), player.clone()) {
        check_dismount(server, &player, &ghast)
    } else {
        Err("Could not mount the Happy Ghast".into())
    };
    ghast_base.remove();
    player.get_entity().set_pos(original_pos);
    result?;
    summary_lines.push("[PASS] bot:happy_ghast_dismount_uses_last_vehicle_move".into());
    Ok(())
}

fn check_dismount(
    server: &Arc<Server>,
    player: &Player,
    ghast: &HappyGhastEntity,
) -> Result<(), String> {
    let ghast_base = ghast.get_entity();
    let player_base = player.get_entity();
    if player.client.java().is_none()
        || ghast
            .get_controlling_passenger()
            .is_none_or(|rider| rider.get_entity().entity_id != player_base.entity_id)
        || !player_base.has_vehicle()
        || player_base.is_sneaking()
        || pending_teleport(player).is_some()
    {
        return Err("Expected a Java driver with no sneak input or pending teleport".into());
    }

    let rise = 0.25;
    let target = ghast_base.pos.load().add_raw(0.0, rise, 0.0);
    let exit = target.add_raw(0.0, f64::from(ghast_base.height()), 0.0);
    // Java sends its changed input before the vehicle move in the same tick.
    queue_packet(
        player,
        &SPlayerInput {
            input: SPlayerInput::JUMP | SPlayerInput::SNEAK,
        },
    )?;
    queue_packet(
        player,
        &SMoveVehicle {
            x: target.x,
            y: target.y,
            z: target.z,
            yaw: ghast_base.yaw.load(),
            pitch: ghast_base.pitch.load(),
            on_ground: false,
        },
    )?;
    let tick_before = ghast_base.tick_count.load(Ordering::Relaxed);
    server.tick();
    if ghast_base.tick_count.load(Ordering::Relaxed) == tick_before {
        return Err("The Happy Ghast was outside the ticking area".into());
    }

    let actual = ghast_base.pos.load();
    if actual != target {
        return Err(format!(
            "Happy Ghast lost the final move on the dismount tick: expected {target:?}, got {actual:?}"
        ));
    }
    if player_base.has_vehicle() || ghast_base.has_passengers() {
        return Err("Rider remained attached after the dismount tick".into());
    }
    let (teleport_id, position) =
        pending_teleport(player).ok_or("Dismount did not send a teleport")?;
    if position != exit || player.position() != exit {
        return Err(format!(
            "Dismount should put the rider on the moved Happy Ghast: expected {exit:?}, got teleport to {position:?} and rider at {:?}",
            player.position()
        ));
    }

    confirm_teleport(player, teleport_id, position)?;
    queue_packet(player, &SPlayerInput { input: 0 })?;
    player.process_inbound_packets();
    check_platform_contact(player, ghast, exit)
}

fn check_platform_contact(
    player: &Player,
    ghast: &HappyGhastEntity,
    exit: Vector3<f64>,
) -> Result<(), String> {
    let entry = exit.add_raw(0.0, -0.125, 0.0);
    for _ in 0..2 {
        queue_packet(
            player,
            &SPlayerPosition {
                position: entry,
                collision: 0,
            },
        )?;
        player.process_inbound_packets();
        let (teleport_id, position) = pending_teleport(player)
            .ok_or("Movement into the ghast did not trigger a correction")?;
        if position != exit || player.position() != exit {
            return Err("Movement into the ghast did not keep the rider on top".into());
        }
        confirm_teleport(player, teleport_id, position)?;
    }

    let edge = ghast.get_entity().bounding_box.load().max.x;
    let departure = Vector3::new(
        edge + f64::from(player.get_entity().width()),
        entry.y,
        entry.z,
    );
    queue_packet(
        player,
        &SPlayerPosition {
            position: departure,
            collision: 0,
        },
    )?;
    player.process_inbound_packets();
    if player.position() != departure || pending_teleport(player).is_some() {
        return Err("Movement off the side of the ghast was not accepted".into());
    }
    Ok(())
}

fn pending_teleport(player: &Player) -> Option<(VarInt, Vector3<f64>)> {
    *player
        .awaiting_teleport
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn confirm_teleport(
    player: &Player,
    teleport_id: VarInt,
    position: Vector3<f64>,
) -> Result<(), String> {
    let player_base = player.get_entity();
    queue_packet(
        player,
        &SConfirmTeleport {
            teleport_id,
            position,
            yaw: player_base.yaw.load(),
            pitch: player_base.pitch.load(),
        },
    )?;
    player.process_inbound_packets();
    if pending_teleport(player).is_some() {
        return Err("Teleport confirmation did not clear the pending teleport".into());
    }
    Ok(())
}

fn queue_packet<P: ClientPacket>(player: &Player, packet: &P) -> Result<(), String> {
    let mut payload = Vec::new();
    packet
        .write_packet_data(&mut payload, &CURRENT_MC_VERSION)
        .map_err(|error| format!("Failed to serialize Happy Ghast test packet: {error:?}"))?;
    player.inbound_packets.push(RawPacket {
        id: P::to_id(CURRENT_MC_VERSION),
        payload: payload.into(),
    });
    Ok(())
}

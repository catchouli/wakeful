// The battle HUD: a status panel for everyone on the field and the
// floating damage numbers. Runs every tick while a battle is up; does
// nothing otherwise. Battle visuals are script territory — the engine
// only stages the fight and sequences turns.
fn on_update(dt) {
    if in_battle() != true {
        ui_close("battle-status");
        return;
    }

    ui_window("battle-status", 8.0, 176.0, 192.0, 56.0);
    let folks = battle_participants();
    let row = 8.0;
    for p in folks {
        ui_text("battle-status", p.id, 8.0, row);
        if p.max_hp != () && p.max_hp > 0 {
            ui_bar("battle-status", 70.0, row + 2.0, 90.0, p.hp / p.max_hp);
        }
        row = row + 14.0;
    }

    // Floating damage numbers: entries the battle script pushed into
    // the battle store, rendered at the participant's screen position
    // and floated up by age. Pure script — the engine only knows how
    // to project a battle point onto the virtual screen.
    let floats = battle_get("floats");
    if floats != () {
        let kept = [];
        for f in floats {
            let age = f.age + dt;
            if age < 1.2 {
                let at = battle_screen_pos(f.x, f.y, f.z);
                if at.onscreen == true {
                    ui_window("float" + f.n, at.x - 12.0, at.y - 14.0 - age * 26.0, 34.0, 14.0);
                    ui_text("float" + f.n, f.text, 4.0, 3.0);
                }
                f.age = age;
                kept.push(f);
            }
        }
        battle_set("floats", kept);
    }
}

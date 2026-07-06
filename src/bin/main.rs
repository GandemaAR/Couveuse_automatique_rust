#![no_std]  // Pas de bibliothèque standard : on tourne sur un micro-contrôleur (pas d'OS)
#![no_main] // Pas de fonction `main()` classique, on utilise celle fournie par esp-hal via #[main]

// Ces deux lignes interdisent certaines pratiques dangereuses en embarqué :
#![deny(
    clippy::mem_forget,
    reason = "mem::forget est risqué avec les types esp_hal, notamment ceux qui gardent \
    des buffers pendant un transfert de données."
)]
#![deny(clippy::large_stack_frames)] // Empêche les frames de pile trop grosses (mémoire limitée)

use esp_hal::{
    main,
    clock::CpuClock,                              // Pour régler la vitesse du processeur
    time::{Duration, Instant},                     // Pour gérer des délais/mesures de temps
    delay::Delay,                                   // Le "délai bloquant" fourni par esp-hal
    gpio::{DriveMode, Flex, Level, OutputConfig, Pull} // Types pour configurer une broche GPIO
};
use esp_println::println; // Permet d'afficher du texte sur le port série (comme printf)

// Crate qui sait parler au capteur de température DS18B20 via le protocole 1-Wire
use ds18b20::Ds18b20;

// Anciens traits (v0.2) d'embedded-hal, nécessaires car `one-wire-bus` ne connaît
// pas encore les traits plus récents (v1.0) qu'utilise esp-hal.
use embedded_hal::{
    blocking::delay::DelayUs, 
    digital::v2::{InputPin, OutputPin}
};

// Le "pont" qui convertit les traits modernes (esp-hal / embedded-hal 1.0)
// vers les anciens traits (embedded-hal 0.2) attendus par one-wire-bus / ds18b20
use embedded_hal_compat::ReverseCompat;

use one_wire_bus::{Address, OneWire}; // Le bus 1-Wire lui-même + le type d'adresse d'un capteur
use core::fmt::Debug;

// En cas de panique (erreur fatale), on boucle à l'infini au lieu de planter :
// c'est la version la plus simple d'un panic handler pour l'embarqué.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

// Génère les métadonnées d'application requises par le bootloader ESP-IDF
esp_bootloader_esp_idf::esp_app_desc!();

/// Parcourt le bus 1-Wire et affiche l'adresse de chaque capteur trouvé.
/// Utile pour découvrir l'adresse exacte de ton DS18B20 la première fois
/// (avant de la coder en dur dans `address_1`).
fn find_devices<P, E>(delay: &mut impl DelayUs<u16>, one_wire_bus: &mut OneWire<P>)
where
    P: OutputPin<Error = E> + InputPin<Error = E>, // Le pin doit pouvoir lire ET écrire
    E: Debug,                                      // Le type d'erreur doit être affichable
{
    for device_address in one_wire_bus.devices(false, delay) {
        // La recherche peut échouer à tout moment, donc on vérifie chaque résultat.
        // L'itérateur s'arrête automatiquement dès qu'une erreur survient.
        let device_address = device_address.unwrap();
        println!(
            "Found device at address {:?} with family code: {:#x?}",
            device_address,
            device_address.family_code() // Le "family code" identifie le type de capteur
        );
    }
}

#[allow(
    clippy::large_stack_frames,
    reason = "il est normal d'allouer des buffers plus gros dans main"
)]
#[main]
fn main() -> ! {
    // --- Initialisation générale du chip ---
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max()); // CPU à vitesse max
    let peripherals = esp_hal::init(config); // Récupère l'accès à tous les périphériques (GPIO, etc.)

    // Le delay d'esp-hal parle "embedded-hal 1.0" ; .reverse() le fait parler "0.2"
    // pour qu'il soit compatible avec ce qu'attend one-wire-bus / ds18b20.
    let mut delay = Delay::new().reverse();

    // --- Configuration de la broche utilisée pour le bus 1-Wire ---
    // Le 1-Wire utilise un seul fil pour parler ET écouter : il faut donc du "open-drain"
    // (la broche peut tirer la ligne vers le bas, mais jamais la forcer vers le haut ;
    // c'est une résistance de pull-up qui la remonte).
    let od_config = OutputConfig::default()
        .with_drive_mode(DriveMode::OpenDrain)
        .with_pull(Pull::Up); // Pull-up interne. Remplace par Pull::None si tu as une résistance externe de 4.7kΩ

    // `Flex` = une broche qui peut être à la fois entrée et sortie (ce dont 1-Wire a besoin)
    let mut one_wire_pin = Flex::new(peripherals.GPIO4);
    one_wire_pin.apply_output_config(&od_config); // Applique le mode open-drain
    one_wire_pin.set_input_enable(true);          // Active la lecture
    one_wire_pin.set_output_enable(true);          // Active l'écriture
    one_wire_pin.set_level(Level::High);           // Ligne au repos = état haut

    // Comme pour le delay : on convertit le pin (traits 1.0) vers les traits 0.2
    // attendus par one-wire-bus. `.reverse_cell()` (et non `.reverse()`) car un pin
    // doit implémenter à la fois InputPin ET OutputPin via la même valeur partagée.
    let one_wire_pin = one_wire_pin.reverse_cell();

    // Construit le bus 1-Wire à partir du pin configuré. C'est CE bus qu'on
    // réutilisera partout ensuite (pas le pin brut, qui est maintenant "avalé").
    let mut one_wire_bus = OneWire::new(one_wire_pin).unwrap();

    // Adresse unique du capteur DS18B20 (trouvée au préalable avec find_devices,
    // ou lue directement sur le boîtier du capteur).
    let address_1 = Address(0x6B000000363E8B28);

    // Crée l'objet représentant CE capteur précis (identifié par son adresse).
    // Les opérations GPIO d'esp-hal ne peuvent pas échouer, d'où `Infallible`.
    let ds18b20_0 = Ds18b20::new::<core::convert::Infallible>(address_1).expect("erreur");

    // Scanne une fois le bus au démarrage pour lister les capteurs présents (debug/vérification).
    find_devices(&mut delay, &mut one_wire_bus);

    loop {
        // 1. Demande au capteur de démarrer une mesure de température.
        //    (Ne bloque pas : le capteur mesure en tâche de fond.)
        ds18b20_0
            .start_temp_measurement(&mut one_wire_bus, &mut delay)
            .unwrap();

        // 2. Lit le résultat de la mesure précédente.
        let ds18b20_data = ds18b20_0.read_data(&mut one_wire_bus, &mut delay);

        match ds18b20_data {
            Ok(valeur) => println!("TEMP: {:?}", valeur.temperature), // Affiche la température lue
            Err(_) => {} // En cas d'erreur de lecture, on ignore silencieusement (à améliorer si besoin)
        }

        // Petite pause de 750ms avant le prochain cycle mesure/lecture.
        let delay_start = Instant::now();
        while delay_start.elapsed() < Duration::from_millis(750) {}
    }
}        
//    ⚠️ Attention : le capteur a besoin d'un certain temps pour terminer
//    sa mesure (jusqu'à 750ms en résolution 12 bits) avant que la lecture
//    soit fiable — voir la remarque plus bas.

#![no_std]  // Pas de bibliothèque standard : on tourne sur un micro-contrôleur (pas d'OS)
#![no_main] // Pas de fonction `main()` classique, on utilise celle fournie par esp-hal via #[main]
#![feature(impl_trait_in_assoc_type)]
// Ces deux lignes interdisent certaines pratiques dangereuses en embarqué :
#![deny(
    clippy::mem_forget,
    reason = "mem::forget est risqué avec les types esp_hal, notamment ceux qui gardent \
    des buffers pendant un transfert de données."
)]
#![deny(clippy::large_stack_frames)] // Empêche les frames de pile trop grosses (mémoire limitée)

use esp_hal::{
    clock::CpuClock,                              // Pour régler la vitesse du processeur
    time::{Instant, Rate},                     // Pour gérer des délais/mesures de temps
    timer::timg::TimerGroup, 
    delay::Delay,                                   // Le "délai bloquant" fourni par esp-hal
    gpio::{DriveMode, Flex, Level, Output, OutputConfig, Pull}, // Types pour configurer une broche GPIO
    i2c::master::{I2c, Config as I2cConfig}
};
use esp_rtos::main;
use embassy_time::{Duration, Timer};
use esp_println::println; // Permet d'afficher du texte sur le port série (comme printf)
// Crate qui sait parler au capteur de température DS18B20 via le protocole 1-Wire
use ds18b20::Ds18b20;
//DHT11
// use dht11::Dht11;
use embedded_dht_rs::dht11::Dht11;

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
use core::fmt::{Debug, Write};
use heapless::String;

// OLED
use ssd1306::{I2CDisplayInterface, Ssd1306Async, prelude::*};

// Embedded Graphics
use embedded_graphics::{
    mono_font::{MonoTextStyleBuilder, ascii::FONT_6X10, MonoTextStyle},
    pixelcolor::BinaryColor,
    prelude::Point,
    prelude::*,
    primitives::{
        Circle, PrimitiveStyle, PrimitiveStyleBuilder, Rectangle, StrokeAlignment, Triangle,
    },
    text::{Alignment, Baseline, Text},
    mock_display::MockDisplay,
};
use profont::{PROFONT_7_POINT,PROFONT_9_POINT,};


// En cas de panique (erreur fatale), on boucle à l'infini au lieu de planter :
// c'est la version la plus simple d'un panic handler pour l'embarqué.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        println!("Error");
    }
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


#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) {
    // --- Initialisation générale du chip ---
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max()); // CPU à vitesse max
    let peripherals = esp_hal::init(config); // Récupère l'accès à tous les périphériques (GPIO, etc.)

    let timg0 = TimerGroup::new(peripherals.TIMG0);

    use esp_hal::interrupt::software::SoftwareInterruptControl;
    let software_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, software_interrupt.software_interrupt0);

    let _ = spawner;    // Le delay d'esp-hal parle "embedded-hal 1.0" ; .reverse() le fait parler "0.2"
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
    let ds18b20_1 = Ds18b20::new::<core::convert::Infallible>(address_1).expect("erreur");

    // Scanne une fois le bus au démarrage pour lister les capteurs présents (debug/vérification).
    find_devices(&mut delay, &mut one_wire_bus);

    // --- Seuils de température pour la couvaison des œufs de poule ---
    // Zone de tolérance : entre TEMP_MIN et TEMP_MAX, aucun système n'est actionné.
    let TEMP_MIN: f32 = 37.5;
    let TEMP_MAX: f32 = 37.8;

    // Les broches 2 zt 5 sont utilisées pour le controle du système de 
    // refroidissement et de rechauffement 
    let pin_de_refroidissement = peripherals.GPIO2;
    let pin_de_rechauffement = peripherals.GPIO5;
    let mut refroidissement = Output::new(pin_de_refroidissement, Level::High, OutputConfig::default());
    let mut rechauffement = Output::new(pin_de_rechauffement, Level::High, OutputConfig::default());

    // --- Configuration du DHT11 ---
    // On utilise une config OutputConfig dédiée (dht11_config) plutôt que de réutiliser
    // od_config (celle du 1-Wire) : ça permet de régler indépendamment le pull-up du DHT11
    // si jamais on doit ajuster ce capteur sans toucher au réglage du DS18B20. 
    let dht11_config = OutputConfig::default()
        .with_drive_mode(DriveMode::OpenDrain)
        .with_pull(Pull::Up); // Pull-up interne. Remplace par Pull::None si tu as une résistance externe de 4.7kΩ

    let mut dht11_pin = Flex::new(peripherals.GPIO27);
    dht11_pin.apply_output_config(&dht11_config); // Applique le mode open-drain
    dht11_pin.set_input_enable(true);          // Active la lecture
    dht11_pin.set_output_enable(true);    
    dht11_pin.set_level(Level::High);           // Ligne au repos = état haut
                                                
    // Contrairement au DS18B20, le crate `embedded_dht_rs` attend directement les traits
    // embedded-hal 1.0 (pas les traits v2 de 0.2.x) : pas besoin de `.reverse_cell()` ici.
    // (Tentative laissée en commentaire pour mémoire : ça ne compilerait pas / n'est pas nécessaire.)
    // let dht11_pin = dht11_pin.reverse_cell();
    // let mut dht11 = Dht11::new(dht11_pin); // ancienne API sans delay (crate `dht11`, abandonnée)

//    let dht11_pin = dht11_pin.reverse_cell();
//    let mut dht11 = Dht11::new(dht11_pin);
    let delay2 = Delay::new();
    let mut dht11 = Dht11::new(dht11_pin, delay2);


    //OLED 
    let i2c_bus = I2c::new(
        peripherals.I2C0,
        // I2cConfig is alias of esp_hal::i2c::master::I2c::Config
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_scl(peripherals.GPIO18)
    .with_sda(peripherals.GPIO23)
    .into_async();

    let interface = I2CDisplayInterface::new(i2c_bus);
    // initialize the display
    let mut display = Ssd1306Async::new(interface, DisplaySize128x64, DisplayRotation::Rotate0)
        .into_buffered_graphics_mode();
    display.init().await.unwrap();

    let text_style = MonoTextStyleBuilder::new()
        .font(&PROFONT_9_POINT)
        .text_color(BinaryColor::On)
        .build();


    Text::with_baseline("Initialisation ...", Point::new(10, 30), text_style, Baseline::Top)
        .draw(&mut display)
        .unwrap();
    
    display.flush().await.unwrap();
    Timer::after(Duration::from_secs(3)).await;
     let _ = display.clear(BinaryColor::Off);
    
    let _ = display.flush().await.unwrap();

    loop {
        let _ = display.clear(BinaryColor::Off);
        println!("test");
        Text::new("   Couveuse   ", Point::new(20, 16), text_style)
                   .draw(&mut display)
                   .unwrap();
        // 1. Demande au capteur de démarrer une mesure de température.
        //    (Ne bloque pas : le capteur mesure en tâche de fond.)
        ds18b20_1
            .start_temp_measurement(&mut one_wire_bus, &mut delay)
            .unwrap();

        // 2. Lit le résultat de la mesure précédente.

        let ds18b20_1_data = ds18b20_1.read_data(&mut one_wire_bus, &mut delay);

        match ds18b20_1_data {
            Ok(valeur) => {
                let mut mes: String<32> = String::new();
                let _ = write!(mes, "Temp: {:.2}°C",valeur.temperature);
                let result: &str = mes.as_str();
                Text::new(result, Point::new(20, 36), text_style)
                   .draw(&mut display)
                   .unwrap();

                 // Trop froid : on coupe le refroidissement et on active le chauffage.
                if valeur.temperature < TEMP_MIN {
                    refroidissement.set_high(); // relais au repos (NC) = refroidissement OFF
                    //println!("TEMP= {}°C. Déactivation du système de refroidissement", valeur.temperature); 
                    rechauffement.set_low();    // relais activé (NO) = chauffage ON
                    //println!("TEMP= {}°C. Activation du système de rechauffement", valeur.temperature); 
                }                

                // Trop chaud : on coupe le chauffage et on active le refroidissement.
                if valeur.temperature > TEMP_MAX {
                    rechauffement.set_high();   // relais au repos (NC) = chauffage OFF
                    //println!("TEMP= {}°C. Déactivation du système de rechauffement", valeur.temperature); 
                    refroidissement.set_low();  // relais activé (NO) = refroidissement ON
                    //println!("TEMP= {}°C. Activation du système de refroidissement", valeur.temperature); 
                }

                // Dans la plage idéale : les deux systèmes restent au repos.
                if valeur.temperature < TEMP_MAX && valeur.temperature > TEMP_MIN {
                    rechauffement.set_high();
                    refroidissement.set_high();
                }
            }
            Err(_) => {
                Text::new("Error", Point::new(20, 36), text_style)
                   .draw(&mut display)
                   .unwrap();
            }
        }

        // Tentative précédente avec une autre méthode du driver (perform_measurement),
        // remplacée par .read() qui est l'API actuelle du crate embedded_dht_rs.
        // match dht11.perform_measurement(&mut delay) {
        //     Ok(meas) => println!("Temp: {} Hum: {}", meas.temperature, meas.humidity),
        //     Err(e) => println!("Error: {:?}", e),
        // };
        match dht11.read() {
            Ok(sensor_reading) => {
                //println!("DHT 11 Sensor - Temperature: {} °C, humidity: {} %",sensor_reading.temperature,sensor_reading.humidity);
                let mut mes: String<32> = String::new();
                let _ = write!(mes, "Humidité: {}%",sensor_reading.humidity);
                let result: &str = mes.as_str();                
                Text::with_baseline(result, Point::new(20, 40), text_style, Baseline::Top)
                   .draw(&mut display)
                   .unwrap();
            }
            Err(error) => {
                Text::new("Error", Point::new(20, 40), text_style)
                   .draw(&mut display)
                   .unwrap();
                //println!("An error occurred while trying to read sensor: {:?}", error),
            }
        }
        display.flush().await.unwrap();
        Timer::after(Duration::from_secs(2)).await;
        let _ = display.clear(BinaryColor::Off);
    }
}        
//    ⚠️ Attention : le capteur a besoin d'un certain temps pour terminer
//    sa mesure (jusqu'à 750ms en résolution 12 bits) avant que la lecture
//    soit fiable — voir la remarque plus bas.

//    ⚠️ Attention : le relais utilisé pour commnadé le système de refroidissement ou de
//    rechauffement est le "JQC3F-05VDC-C". Il est commandé pour un signal baw(low)
//    INPUT=0 ---> Basculement de NC(Normaly Close) à NO(Normaly Open)
//    INPUT=1 ---> Basculement de NO(Normaly Open) à NC(Normaly Close)


//    Schema de montage wokwi : https://wokwi.com/projects/468817331616684033

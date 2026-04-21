# NEAMPMOD The Tweed

The Tweed is a circuit-level simulation inspired by the 1957 Fender® 5E3 Deluxe amplifier.

* The `stock` v1a/b preamp tube is a General Electric 12AY7 100k (spline)
* The `mod` v1a/b preamp tube is a RCA 12AX7 100k (spline)
* The v2a voltage gain tube is a RCA 12AX7 tube 100k (spline)
* The phaser inverter stage uses a General Electric 12AX7 56k tube (spline)
* The v3/v4 power amp stage uses a RCA 6V6GT tube + 5e3 configuration (spline)
* The v5 rectifier stage uses a Generic 5Y3 (Koren)
  * Koren fitting based off of General Electric 5Y3 documentation
* Speaker impedence modelling assumes a Jensen® P12R speaker.

<div style="text-align: center;">
    <img width="50%" src="img/amp.png">
</div>
<div style="text-align: center;">
    <img width="50%" src="img/controls.png">
</div>

## Controls

### Channel Toggle

Toggles between the two available channels, or jumpers channels.

* `N` = Normal channel
* `J` = Channels jumpered
* `B` = Bright channel

Volume controls are always active and interactive even if not jumpered, and/ or
the channel is not active.

### Power Toggle

Turns the amp DSP `on`/ `off` - Note the plugin does not passthru signal when `off`.

### Tone

Controls the tone; This has a large impact on drive.

### Bright Channel Volume

Drives `v1b` and has  `500 nF` bright cap; Channel has more drive and treble than `Normal` channel.

### Normal Channel Volume

Drives `v1a` and is warmer/ thicker than the `Bright` channel.

### AY|AX Toggle

Toggles between a `12AY7` and `12AX7` in the V1 tube socket; Provides earlier and more aggressive breakup.

### Master Knob

Linear fine-tuning volume control at end of circuit after IR, this does not impact gain / tone.

### IR Load (Browse Button)

Opens an OS-native file window, navigate to your IR WAV file and load it.

NOTE: The included `default.wav` has audio artifacts/ is low-quality and should be replaced, see below for suggestions on free IRs.

### Input / Output Trim

See the `Gain Setup` section.

### View

Switches between viewing the front of the amplifiers and the amplifiers top control panel.

### Circuit Stats

Shows the simulated voltage levels within the amp, the `V1`, `V2`, and `v3/v4` are the B+ plate voltages.

## Using the Plugin

The Tweed is available in VST3 and CLAP plugin formats for Linux and Windows.

To install the plugins copy the `.vst3` to your VST3 directory, and likewise to your `.clap` directory for
the CLAP plugin.

The plugin includes a `default.wav` IR file, I strongly suggest loading a higher quality IR file to get
the best out of the plugin; The following sources provide excellent impulse response files:

* [Origin Effects IR Cab Library](https://origineffects.com/product/ir-cab-library/)
* [Tone3000](https://tone3000.com/)

### Tone3000 IR Files

I would suggest searching for Fender / Jensen IRs on [Tone3000](https://tone3000.com/), there are a range of high-quality IRs with multiple microphones, and microphone positions.

## Gain Setup

The `Signal` level meter displays the signal voltage as the simulated amplifier's input jack would see it. 

Use this to calibrate your signal chain to the physically correct operating range.

Expected voltage ranges by pickup type:

* Passive single-coils: 80 - 150mV moderate playing, 200–350mV hard attack
* Passive humbuckers: 150 - 350mV moderate playing, 400–700mV hard attack
* Active pickups: 500mV - 1.5V

### Calibration workflow

* Set your interface gain so hard playing peaks are comfortable and well below the clip LED — around -12 to -18 dBFS in your DAW if visible
* Play normally across your full dynamic range
* Use the input trim to bring the meter into the expected range for your pickup type
  * If the signal sits consistently above the expected range, reduce trim — you are driving the first tube stage harder than the real circuit would be driven
  * If it sits below, increase trim or add interface gain

Where the signal lands on the meter determines where `V1A` operates on its transfer curve — too high and the amp
will behave as if a boost pedal is already in the chain; too low and you will lose the touch sensitivity that emerges 
near the operating point.

## Reporting Issues

Please raise a GitHub issue with the following:

* Hardware and OS information
* Digital Audio Workstation (DAW) and version
* Description of issue
* Description of how to reproduce the issue

## Links to Useful Information

A great deal of information is avilable online regarding amplifier building, physics and designs, some of the links have provided invaluable information and insight:

* [ampbooks](https://www.ampbooks.com/)
* [robrobinette](https://robrobinette.com/)
* [helmutkelleraudio](https://www.helmutkelleraudio.de/)
* [diyaudio](https://www.diyaudio.com/)

## Author

* Daniel Wray

## License and Legal Information

This code is released under the [GNU GPLv3 license](LICENSE).

The binaries (VST3, CLAP) arereleased under a [Freeware EULA license](BINARY_LICENSE).

* Fender® is a registrated trademark of Fender Musical Instruments Corporation.
* VST® is a registered trademark of Steinberg Media Technologies GmbH.


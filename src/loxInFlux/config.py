import os
from dataclasses import dataclass, field
from typing import Dict, Any, List, Optional
import tomli
from loxInFlux.argparser import get_args
@dataclass
class LoggingConfig:
    level: str = "INFO"

@dataclass
class MiniserverConfig:
    host: str = "0.0.0.0"
    port: int = 80
    user: str = "admin"
    password: str = "admin"
    visu_password: str = None
    max_reconnect_attempts: int = 0 # Optional (Default: 0): number of retries to establish connection to miniserver if connection is lost. Not set / set to 0 will result in unlimited retries - Default 0
    
@dataclass
class PathsConfig:
    data_dir: str = "data"

@dataclass
class MQTTConfig:
    host: str = "localhost"
    port: int = 1883
    username: Optional[str] = None
    password: Optional[str] = None
    topic: bytes = b"loxone/metrics"
    client_id: str = "loxinflux"

@dataclass
class TelegrafConfig:
    host: str = "0.0.0.0"
    port: int = 8086
    protocol: str = "udp"  # Can be "udp", "tcp", "execd", or "mqtt"
    mqtt: MQTTConfig = field(default_factory=MQTTConfig)
    max_retries: int = 100

    @classmethod
    def from_dict(cls, config_dict: Dict[str, Any]) -> 'TelegrafConfig':
        """Create TelegrafConfig from dictionary."""
        # Only create MQTT config if protocol is mqtt
        mqtt_config = None
        if config_dict.get('protocol') == 'mqtt':
            mqtt_config = MQTTConfig(**config_dict.get('mqtt', {}))
        else:
            mqtt_config = MQTTConfig()  # Use defaults for non-MQTT protocols
        
        return cls(
            host=config_dict.get('host', "0.0.0.0"),
            port=config_dict.get('port', 8086),
            protocol=config_dict.get('protocol', "udp"),
            mqtt=mqtt_config,
            max_retries=config_dict.get('max_retries', 100)
        )

@dataclass(frozen=True)
class FilterConfig:
    type_blacklist: List[str] = field(default_factory=list)
    type_whitelist: List[str] = field(default_factory=list)
    uuid_blacklist: List[str] = field(default_factory=list)
    uuid_whitelist: List[str] = field(default_factory=list)

    def __post_init__(self):
        # Since the class is frozen, we need to use object.__setattr__ to modify fields
        object.__setattr__(self, 'type_blacklist', set(map(str.upper, self.type_blacklist)))
        object.__setattr__(self, 'type_whitelist', set(map(str.upper, self.type_whitelist)))
        object.__setattr__(self, 'uuid_blacklist', set(self.uuid_blacklist))
        object.__setattr__(self, 'uuid_whitelist', set(self.uuid_whitelist))

    def should_include_control(self, control_type: str, uuid: str) -> bool:
        """Determine if a control should be included based on the filter rules.
        
        Args:
            control_type: The type of the control
            uuid: The UUID of the control
            
        Returns:
            bool: True if the control should be included, False otherwise
        """
        control_type = control_type.upper()
        
        # If whitelists are not empty, only include items in the whitelist
        if self.type_whitelist:
            if control_type not in self.type_whitelist:
                return False
        elif control_type in self.type_blacklist:
            return False
            
        if self.uuid_whitelist:
            return uuid in self.uuid_whitelist
        return uuid not in self.uuid_blacklist

    def filter_controls(self, controls: Dict[str, Any]) -> Dict[str, Any]:
        """Filter a dictionary of controls based on the filter configuration.
        
        Args:
            controls: Dictionary of controls where each control has at least
                    'type' in its data and optionally 'parent_uuid' to indicate subcontrols
                    
        Returns:
            Dictionary containing only the controls that pass the filter rules,
            including parent-child relationship checks
        """
        filtered = {}
        
        def should_include(uuid: str, control: Dict) -> bool:
            # Get control type
            control_type = control['type']
            
            # Check if control passes basic filter
            if not self.should_include_control(control_type, uuid):
                return False
                
            # For subcontrols, check if parent is included
            if 'parent_uuid' in control:
                parent_uuid = control['parent_uuid']
                parent = controls.get(parent_uuid)
                if not parent:
                    return False
                if not self.should_include_control(parent['type'], parent_uuid):
                    return False
                    
            return True

        # First add all non-subcontrols that pass the filter
        for uuid, control in controls.items():
            if 'parent_uuid' not in control and should_include(uuid, control):
                filtered[uuid] = control

        # Then add all subcontrols whose parents were included
        for uuid, control in controls.items():
            if 'parent_uuid' in control and should_include(uuid, control):
                filtered[uuid] = control

        return filtered

@dataclass
class ControlConfig:
    type_blacklist: List[str] = field(default_factory=list)
    websocket: FilterConfig = field(default_factory=FilterConfig)
    grabber: FilterConfig = field(default_factory=FilterConfig)

    def __post_init__(self):
        # Convert main type_blacklist to set and uppercase
        self.type_blacklist = set(map(str.upper, self.type_blacklist))

@dataclass
class GeneralConfig:
    grabber: bool = True
    round_floats: bool = False
    rounding_precision: int = 5
    grabber_interval: int = 300  # 5 minutes in seconds

@dataclass
class AppConfig:
    general: GeneralConfig
    logging: LoggingConfig
    miniserver: MiniserverConfig
    paths: PathsConfig
    telegraf: TelegrafConfig
    control: ControlConfig

    @classmethod
    def load(cls, config_dir: str = None, data_dir: str = None) -> 'AppConfig':
        """Load configuration from config/config.toml file.
        
        Args:
            config_dir: Optional directory containing config.toml
            data_dir: Optional directory for storing miniserver data
        """
        args = get_args()
        config_dir = args.config_dir
        data_dir = args.data_dir
        
        # Default config path if not specified
        default_config_dir = "config"
        config_dir = config_dir or default_config_dir
        
        # Konvertiere relative Pfade zu absoluten wenn nötig
        if not os.path.isabs(config_dir):
            config_dir = os.path.abspath(config_dir)
            
        # Stellen Sie sicher, dass die Verzeichnisse existieren
        os.makedirs(config_dir, exist_ok=True)
        
        config_path = os.path.join(config_dir, "config.toml")
        if not os.path.exists(config_path):
            raise FileNotFoundError(f"Config file not found at {config_path}")
        
        with open(config_path, "rb") as f:
            config_dict = tomli.load(f)
        
        # Override data_dir if specified
        if data_dir:
            # Konvertiere data_dir zu absolutem Pfad wenn nötig
            if not os.path.isabs(data_dir):
                data_dir = os.path.abspath(data_dir)
            config_dict.setdefault('paths', {})['data_dir'] = data_dir
        
        # Create FilterConfig instances for websocket and grabber
        filters_config = config_dict.get('filters', {})
        websocket_config = FilterConfig(**filters_config.get('websocket', {}))
        grabber_config = FilterConfig(**filters_config.get('grabber', {}))
        
        return cls(
            general=GeneralConfig(**config_dict.get('general', {})),
            logging=LoggingConfig(**config_dict.get('logging', {})),
            miniserver=MiniserverConfig(**config_dict.get('miniserver', {})),
            paths=PathsConfig(**config_dict.get('paths', {})),
            telegraf=TelegrafConfig.from_dict(config_dict.get('telegraf', {})),
            control=ControlConfig(
                type_blacklist=filters_config.get('type_blacklist', []),
                websocket=websocket_config,
                grabber=grabber_config
            )
        )

# Statt direkter Initialisierung eine Funktion für die Initialisierung bereitstellen
config =  AppConfig.load()


# For backwards compatibility
def get(section: str, key: str, default: Any = None) -> Any:
    """Get a configuration value (legacy method)."""
    section_obj = getattr(config, section, None)
    if section_obj is None:
        return default
    return getattr(section_obj, key, default)
-- A synthetic Dolibarr install file for the widget table.
CREATE TABLE llx_widgetshop_widget (
    rowid   integer AUTO_INCREMENT PRIMARY KEY,
    ref     varchar(128) NOT NULL,
    label   varchar(255),
    status  integer DEFAULT 0 NOT NULL
) ENGINE=InnoDB;

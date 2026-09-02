<?php
// A synthetic Dolibarr module descriptor (no real project).
require_once DOL_DOCUMENT_ROOT.'/core/modules/DolibarrModules.class.php';

class modWidgetshop extends DolibarrModules
{
    public function __construct($db)
    {
        $this->db = $db;
        $this->numero = 500100;
        $this->rights_class = 'widgetshop';
        $this->family = 'products';
        $this->module_parts = array('hooks' => array('data' => array('widgetcard')));
    }

    public function init($options = '')
    {
        return $this->_init(array(), $options);
    }
}

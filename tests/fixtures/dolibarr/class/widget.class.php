<?php
// A synthetic Dolibarr business object built on CommonObject.
require_once DOL_DOCUMENT_ROOT.'/core/class/commonobject.class.php';

class Widget extends CommonObject
{
    public $element = 'widget';
    public $table_element = 'widgetshop_widget';

    public function validate($user)
    {
        $this->status = 1;
        $this->call_trigger('WIDGET_VALIDATE', $user);
        return 1;
    }
}
